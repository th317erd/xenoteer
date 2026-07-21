# Product contract and accepted decisions

## 1. Product definition

Xenoteer is a full Linux desktop packaged as a container and controlled through
a bot-oriented API. It combines:

- realistic global keyboard and pointer input at the X server;
- semantic inspection and invocation through AT-SPI;
- screenshots, window metadata, and event-driven waits;
- a human-visible browser viewer;
- reliable action ordering, deadlines, deduplication, and cleanup;
- SDK ergonomics modeled on Puppeteer without pretending desktop UI is a DOM.

The first release targets X11 only. It is a single-desktop runtime, not a fleet
orchestration platform. A higher-level service may run many Xenoteer containers,
but the container does not schedule peers or manage hosts.

## 2. Users and principal workflows

Primary users are automation agents and developers building agent systems.
Secondary users are test engineers, operators diagnosing automation, and humans
observing or explicitly taking over a session.

Required workflows:

1. Start a container and wait for a truthful readiness response.
2. Launch an allowlisted desktop application with a controlled environment.
3. Locate a window and optionally locate an accessible element.
4. Observe state through metadata, AT-SPI, screenshots, or event subscriptions.
5. act through semantic invocation or serialized physical input;
6. wait on explicit postconditions rather than fixed sleeps;
7. inspect a trace that states what was attempted and observed;
8. shut down without leaving child processes or invalid input state.

## 3. Release-one success criteria

Release one is successful when all of these are true:

- A Debian/XFCE desktop boots deterministically under Docker without
  `--privileged`.
- Xvfb is fixed initially at 1920x1080, 24-bit color, and 96 DPI.
- GTK, Qt, Chromium/Electron, and Firefox test fixtures can be launched and
  controlled through at least their supported physical path.
- Standard controls in each supported toolkit are discoverable through AT-SPI
  when the application exposes them.
- Smooth pointer movement, clicks, drags, scrolling, key input, and text input
  pass the conformance matrix.
- A human can view the desktop in a browser without their browser input
  corrupting bot state.
- Disconnects, timeouts, panics in request tasks, and container shutdown cause
  a bounded best-effort reset of pressed input.
- Public commands are versioned, bounded, authenticated when exposed off
  loopback, and have stable machine-readable errors.
- The image has an SBOM, provenance attestation, license manifest, and published
  source correspondence procedure for copyleft packages.

## 4. Accepted technology decisions

| Area | Decision | Reason |
|---|---|---|
| Base | Debian stable, pinned by digest for releases | Broad desktop packages, conservative updates, auditable sources |
| Desktop | XFCE with compositor, power manager, locker, and session restoration disabled | Complete but light desktop; fewer capture and determinism surprises |
| Display | Xvfb | Native X11 virtual framebuffer with mature tooling |
| Supervision | s6-overlay v3 / s6-rc | Correct PID 1 behavior, dependency ordering, readiness, privilege drop |
| Physical input | Native XTEST requests through x11rb | Global input behavior matching device events; no subprocess per action |
| X11 client | x11rb pure-Rust connection | Typed core/extension bindings with no required Xlib control layer |
| Keyboard model | libxkbcommon binding plus X server mapping | Layout/keycode reasoning; XTEST still emits physical keycodes |
| Semantics | Rust `atspi` over zbus/Tokio | Native AT-SPI object model and event stream |
| API | Axum + Tokio, JSON over HTTP/WebSocket | Async Rust ecosystem, typed protocol, streaming events |
| Viewer | x11vnc -> websockify -> noVNC, isolated behind an adapter | Fastest interoperable initial viewer; replaceable because x11vnc is unmaintained |
| Diagnostics | xdotool installed but never used by the daemon's normal path | Oracle and operator tool without inheriting XSendEvent/subprocess limitations |
| Server license | Repository Business Source License with four-year change date | Community source availability while delaying hyperscaler exploitation |
| SDK/schema license | Apache-2.0 in explicitly separated packages | Encourages integration without relicensing the server |

## 5. Closed architectural decisions

### 5.1 Input is an actor, not a shared connection

Exactly one input actor serializes physical actions for a desktop. It owns one
dedicated X connection. Other tasks never lock and use that connection. This
prevents action interleaving and uses X11's same-connection request ordering as
an execution barrier.

Observation, clipboard event handling, and accessibility use independent
connections/tasks. XTEST permits a delay on each fake input request and states
that no later request from that client is processed until it completes; sharing
the connection would turn a smooth mouse path into an observation stall.

### 5.2 Semantic action is not physical action

`element.invoke` calls an AT-SPI action. `element.click` resolves a component
rectangle, moves the physical pointer, and sends XTEST button input. Neither
silently falls back to the other. An explicit `strategy: auto` convenience may
choose, but the result and trace MUST disclose the selected strategy.

### 5.3 Human viewing is not collaborative input

x11vnc receives RFB keyboard and pointer messages directly. Those events bypass
Xenoteer's lease, command IDs, queue, trace, and pressed-state tracker. Therefore
the server-side VNC session is view-only by default. A noVNC client checkbox is
not the enforcement boundary.

A future takeover mode may:

1. stop accepting new bot input;
2. drain or cancel at a safe boundary;
3. reset pressed input;
4. revoke the bot lease;
5. enable RFB input server-side;
6. emit a mode-generation change;
7. reverse those steps before bot control resumes.

Release one does not support simultaneous bot and human input.

### 5.4 References are generation-bound

X11 window IDs can be reused and AT-SPI objects can disappear while a client
holds a reference. Public `WindowRef` and `ElementRef` values carry a
`DesktopGeneration`. Window references additionally preserve identity evidence
such as PID, class, and initial creation observation; element references preserve
application bus name and object path. A mismatch returns `stale_reference`; it
never targets whatever now happens to reuse the identifier.

### 5.5 The API is bounded and explicit

- One controller lease per desktop in release one.
- Every mutating command carries a client-generated command ID.
- A bounded TTL result cache deduplicates retries.
- Non-idempotent commands are never automatically replayed after an ambiguous
  transport failure.
- Queues, messages, screenshots, trees, event subscriptions, logs, and launch
  output all have configured limits.
- Generic shell execution is not a default API capability.

## 6. Defaults to validate before stabilization

| Default | Initial value | Validation |
|---|---:|---|
| Display | 1920x1080x24 at 96 DPI | GTK/Qt/browser rendering fixtures and screenshot dimensions |
| Pointer nominal speed | 1,200 px/s | Hover, menu, canvas, drag, and latency suites |
| Auto move duration | clamp 80-650 ms | Exact endpoint and perceived-continuity tests |
| Pointer sample rate | 60 Hz | Event recorder, CPU use, and missed-hover tests |
| Pre-click dwell | 30 ms | Toolkit double-click/menu behavior |
| Default pointer press duration | 50 ms | Toolkit input recorder matrix |
| Default keyboard hold duration | 35 ms | Toolkit input recorder matrix |
| Input queue capacity | 256 commands | Load/backpressure test; no silent drop |
| Recent command results | 10,000 entries or 15 minutes, first limit wins | Memory and reconnect tests |
| WebSocket application heartbeat | 15 s; stale after 45 s | Proxy and disconnect tests |
| Default action timeout | 30 s; per method overrides allowed | Slow-launch and blocked-app tests |
| `/dev/shm` | 1 GiB minimum documented; 2 GiB recommended for browser-heavy work | Chromium/QtWebEngine stress fixtures |

Initial resource ceilings are also closed defaults, not values for individual
handlers to invent:

| Resource | Initial ceiling/default |
|---|---:|
| Controller lease idle TTL | 60 s |
| HTTP JSON body / reassembled WebSocket message | 1 MiB |
| Clipboard binary upload / selection payload | 16 MiB |
| Inline text or clipboard bytes before SDK artifact promotion | 256 KiB |
| Authenticated command admission | 120/minute/principal, burst 30 |
| Accepted/running commands | 32/principal, 128/daemon |
| WebSocket outbound normal/reserved slots | 1,024 / 32 |
| Event subscriptions | 16/principal |
| Event replay | 10,000 events or 16 MiB, first limit wins |
| AT-SPI snapshot | 10,000 nodes or 16 MiB encoded |
| AT-SPI proxy call / whole query | 2 s / 10 s |
| Screenshot | 16,000,000 input pixels, 32 MiB encoded, queue 16, 1/principal and 2/daemon concurrent |
| Active selection INCR transfers | 2/requestor, 8/daemon |
| Managed application groups | 32/desktop; 10 launches/minute |
| Captured stdout/stderr | 8 MiB each/process, then explicit truncation |
| Private artifacts | 256 artifacts or 1 GiB/desktop; 1 hour TTL |
| Ordinary server wait / HTTP long-poll | 30 s maximum |
| `key.sequence` planned duration / implicit command timeout | 5 min / 305 s |
| Viewer ticket | single use, 60 s TTL |

Authenticated command admission is a token bucket. Health probes have a
separate 60/minute/source bucket and authentication failures a 10/minute/source
bucket; neither consumes command allowance. Limits are per daemon as well as per
principal so minting principals cannot defeat memory protection. Upload and
image-response endpoints apply their explicitly larger binary ceilings without
raising the JSON/WS ceiling.

Defaults are declared in one Rust configuration type, surfaced by diagnostics,
and copied into SDK constants only through generated schema artifacts.

## 7. Compatibility policy

Release-one support levels:

- **Guaranteed:** Debian stable image as published, XFCE session, Xvfb, included
  GTK test application, included Qt test application, packaged Chromium and
  Firefox fixtures.
- **Expected:** arbitrary X11 applications using conventional core input and
  EWMH window behavior.
- **Best effort:** custom canvas/OpenGL widgets, unusual input grabs, legacy
  applications that reject modern clipboard targets, and applications with
  incomplete AT-SPI adapters.
- **Unsupported:** Wayland-native-only applications, kernel/input-device
  injection, multi-seat, host X server control, privileged device passthrough.

AT-SPI absence is not itself an application incompatibility. Physical input and
visual observation remain supported, while semantic calls return an explicit
`unsupported_by_target` or `not_accessible` error.

## 8. Explicitly deferred

- Wayland, XWayland orchestration, and compositor-specific automation
- GPU passthrough and accelerated 3D guarantees
- Audio capture/playback
- Multiple monitors, dynamic RandR resize, rotation, and fractional scaling
- Touch, stylus, gestures, and high-resolution XI2 scrolling
- OCR, image matching, and computer-vision element location
- Macro recording from arbitrary human input
- Multi-controller collaboration
- Fleet scheduling, Kubernetes operators, and cross-container session routing
- Browser-specific DevTools Protocol integration
- Host filesystem browsing outside explicitly mounted workspaces

## 9. Decision-change protocol

Changing a closed decision requires a short decision record appended here with:

- date and author;
- decision being replaced;
- evidence or failure that makes replacement necessary;
- compatibility and migration effects;
- security and licensing effects;
- affected phase gates and tests.

Do not allow the implementation to create an accidental decision by landing a
one-off exception first.

## 10. Primary references

- [XTEST extension protocol](https://www.x.org/releases/X11R7.6/doc/xextproto/xtest.html)
- [X11 request ordering](https://www.x.org/guide/communication/)
- [Extended Window Manager Hints](https://specifications.freedesktop.org/wm/latest-single/)
- [AT-SPI development guide](https://gnome.pages.gitlab.gnome.org/at-spi2-core/devel-docs/index.html)
- [s6-overlay](https://github.com/just-containers/s6-overlay)
- [x11rb documentation](https://docs.rs/x11rb/latest/x11rb/)
- [x11vnc upstream status](https://github.com/LibVNC/x11vnc)
- [noVNC upstream](https://github.com/novnc/noVNC)
