# Xenoteer Implementation Plan

Status: Accepted for implementation
Last updated: 2026-07-20

## 1. Vision

Xenoteer is a bot-controllable, full Linux desktop environment packaged as a
container. It combines semantic UI inspection with realistic physical input,
human-visible remote viewing, and a Puppeteer-style automation API.

The initial target is X11. Wayland is explicitly outside the first release.

## 2. Product Goals

- Boot a deterministic Linux desktop from one container command.
- Let bots observe and control arbitrary X11 desktop applications.
- Provide both semantic AT-SPI automation and physical XTEST input.
- Make the same desktop visible to a human through a browser.
- Support reliable waits and postcondition checks instead of sleep-driven
  scripts.
- Provide a polished asynchronous SDK modeled after Puppeteer's ergonomics.
- Run without a privileged container under normal configurations.
- Keep original Xenoteer code commercially protected under its BSL terms while
  using permissively licensed protocol schemas and client SDKs.

## 3. Selected Technology

### Desktop runtime

- Debian stable base image
- XFCE desktop environment
- Xvfb virtual X11 display
- Fixed initial display profile: 1920x1080, 24-bit color, 96 DPI
- s6-overlay as PID 1 and service supervisor
- System and per-desktop session D-Bus
- AT-SPI accessibility bus and registry
- x11vnc, websockify, and noVNC for human viewing
- xdotool installed as a diagnostic and compatibility tool, not as the daemon's
  control implementation

### Rust control plane

- `xenoteerd`: resident desktop control daemon
- Tokio asynchronous runtime
- Axum HTTP and WebSocket server
- x11rb for X11 core protocol, XTEST, window properties, and extensions
- Rust `atspi` bindings over zbus for semantic accessibility
- xkbcommon for keyboard layouts, keysyms, and Unicode input planning
- Serde-based protocol types and JSON messages
- Structured tracing and metrics

### Developer surface

- `xenoteerctl` diagnostic CLI
- Rust SDK first
- TypeScript SDK second
- Python SDK third
- Apache-2.0 protocol schemas and client SDKs

Dependency versions will be pinned during implementation and recorded in lock
files and image build metadata.

## 4. Architectural Boundaries

```text
Rust / TypeScript / Python SDKs
                |
                v
       HTTP + WebSocket API
                |
                v
        Action coordinator
       /         |          \
      /          |           \
 Input worker  Observation   AT-SPI model
      |          services        |
      |          |               |
 XTEST/X11    X11/EWMH       Accessibility bus
      \          |               /
       \         |              /
         Xvfb + XFCE desktop
                |
        x11vnc / noVNC viewer
```

Important boundaries:

- One serialized input worker owns physical input for each desktop.
- The input worker uses a dedicated X11 connection.
- Observation uses separate connections so an interpolated move does not block
  screenshots, window queries, or accessibility events.
- Window activation uses EWMH messages where appropriate.
- Physical input is sent globally through XTEST after activating the target.
  Xenoteer will not rely on targeted `XSendEvent`, which many applications
  reject.
- AT-SPI invocation and physical input are separate public operations.
- Third-party desktop packages retain their upstream licenses and are not part
  of the BSL-covered Licensed Work.

## 5. Input Model

### 5.1 Serialization and ownership

Each desktop has one ordered action queue and, initially, one exclusive
controller lease. This prevents separate clients from interleaving actions and
creating invalid sequences such as a click in the middle of another client's
drag.

The input worker tracks:

- Pressed mouse buttons
- Pressed keyboard keys
- Active modifiers
- Current action and command identifier
- Expected pointer endpoint
- Control-lease owner

On cancellation, disconnect, timeout, or daemon shutdown, Xenoteer releases all
buttons and keys it believes are pressed. Cleanup is best-effort and produces a
diagnostic event if verification fails.

### 5.2 Command delivery

Input commands are not inherently idempotent. Every mutating command therefore
has a client-generated command ID. The daemon retains a bounded cache of recent
results so retrying a request cannot accidentally type twice or double-click.

Atomic action sequences execute without interleaving. Deadlines apply to queued
and executing work. Cancellation is only acknowledged at safe boundaries where
input state can be restored.

### 5.3 Mouse interpolation

Smooth cursor movement is the default. Instant movement remains available
because intermediate hover events can themselves disrupt hover-sensitive UIs.

Initial movement modes:

- `smooth`: deterministic ease-in/ease-out movement
- `linear`: deterministic constant-velocity movement
- `instant`: one direct movement event
- `relative`: interpolated movement from the current position
- `waypoints`: caller-specified path through multiple points

Initial automatic movement policy:

1. Query the actual pointer immediately before starting.
2. Calculate duration from distance and an initial nominal speed of roughly
   1,200 pixels per second.
3. Clamp automatic duration to approximately 80-650 milliseconds.
4. Sample at 60 Hz.
5. Apply `smoothstep(t) = 3t^2 - 2t^3` for default easing.
6. Round to integer X11 coordinates and remove duplicate samples.
7. Always emit the exact destination as the final event.
8. Issue a same-connection pointer query after the scheduled path.
9. Return both requested and observed endpoints.

These values are starting points and must be validated against the input test
matrix before becoming stable API defaults.

Random movement is excluded initially. A future humanization mode must be
explicitly enabled and seedable so failures remain reproducible.

### 5.4 Click and drag behavior

A physical click consists of:

1. Optional interpolated movement to the target
2. Optional pre-click dwell, initially around 30 ms
3. Button press
4. Configurable press duration
5. Button release
6. Optional post-click settle period

A drag is one atomic action:

1. Move to the start point
2. Press and verify tracked button state
3. Traverse the interpolated path while held
4. Reach and verify the exact endpoint
5. Release the button

Any drag error must attempt a button release before returning.

### 5.5 Keyboard and text

- Key down, key up, press, chords, and paced sequences use XTEST keycodes.
- Modifier-clearing operations restore the prior state afterward.
- Arbitrary text input may choose between keymap-aware XTEST, AT-SPI editable
  text, or clipboard paste according to caller policy and target capability.
- Clipboard insertion must not be presented as physical typing in action traces.
- Temporary keymap changes, if used, are serialized and restored.

## 6. Initial Automation Surface

### Mouse

- Absolute and relative movement
- Smooth, linear, instant, and waypoint movement
- Click and double-click
- Button down and button up
- Atomic drag
- Vertical and horizontal scrolling
- Paced repeated scrolling
- Pointer location and pressed-state inspection

### Keyboard

- Key press, key down, and key up
- Modifier chords
- Paced key sequences
- Text typing
- Clipboard-assisted text insertion
- Modifier clearing and restoration

### Windows

- List and search by title, class, PID, desktop, and visibility
- Active and focused window inspection
- Title, PID, geometry, state, and desktop inspection
- Activate, focus, and raise
- Move and resize
- Minimize, map, and unmap
- Graceful close
- Destructive kill behind an explicit capability
- Wait for creation, visibility, focus, state, or geometry

### Desktop observation

- Full-screen, window, and region screenshots
- Pointer state
- Window tree and stacking order
- Display profile and generation identifier
- CLIPBOARD and PRIMARY get/set
- Window and desktop event subscriptions
- Structured action traces

### Semantic accessibility

- Accessibility tree snapshots
- Incremental cache and event handling
- Selection by role, name, state, application, and ancestry
- Text, value, state, relations, and bounds
- Accessible actions
- Editable text operations
- Semantic focus and visibility
- Semantic waits and subscriptions
- Stale-element detection
- `element.invoke` for direct AT-SPI activation
- `element.click` for AT-SPI resolution followed by physical XTEST input

## 7. Service Startup and Readiness

Expected dependency order:

```text
Xvfb
  -> system/session D-Bus and AT-SPI
    -> XFCE session and window manager
      -> xenoteerd
      -> x11vnc, websockify, and noVNC
```

The container may report process health before it reports desktop readiness.
Readiness requires:

- X11 connection succeeds
- XTEST is available
- Window manager is responsive
- Session D-Bus is responsive
- AT-SPI bus state is known
- Input worker passes a non-mutating self-check

AT-SPI failure should produce a degraded readiness state when raw X11 control
still works, rather than making the entire desktop unreachable.

The desktop processes run as an unprivileged user. s6 may begin as root for
initialization and then start services with explicit privilege separation. The
standard deployment must not require `--privileged`.

Browser-heavy profiles will document or configure an appropriately sized
`/dev/shm`; Docker's small default shared-memory allocation is insufficient for
reliable full-browser sessions.

## 8. Implementation Phases

Each phase ends with a demonstrable artifact and an acceptance gate. A phase is
not complete merely because its source code exists.

### Phase 0: Architecture and repository foundation

Deliverables:

- [ ] Cargo workspace
- [ ] `xenoteerd` crate
- [ ] Shared protocol-types crate
- [ ] `xenoteerctl` crate
- [ ] Integration-test application area
- [ ] Container/image directory
- [ ] Architecture decision record directory
- [ ] Formatting, linting, unit-test, and image-build CI
- [ ] Third-party notice and SBOM structure
- [ ] Initial health request between CLI and daemon

Decisions to record:

- Coordinate spaces and conversion rules
- Window and accessible-element identifiers
- Command IDs and deduplication lifetime
- Error envelope and capability reporting
- Control-lease semantics
- API versioning policy

Acceptance gate:

- The workspace builds and tests in CI.
- The daemon starts and stops cleanly.
- The CLI obtains a typed health response.
- No public endpoint is yet declared stable.

### Phase 1: Native X11 input engine

Deliverables:

- [ ] Dedicated x11rb/XTEST connection
- [ ] Serialized action worker
- [ ] Control ownership and queueing
- [ ] Absolute, relative, smooth, linear, and instant movement
- [ ] Click, button down/up, and double-click
- [ ] Atomic drag
- [ ] Paced discrete scrolling
- [ ] Key down/up, press, and chords
- [ ] Tracked input state and emergency cleanup
- [ ] Final pointer verification
- [ ] GTK or equivalent X11 event-recorder test application

Acceptance gate:

- A 1,000-pixel move produces multiple ordered motion events.
- Requested duration is respected within an agreed tolerance.
- The exact endpoint is reached and reported.
- Intermediate windows receive enter, leave, and motion events.
- Button-down state remains active for the entire drag.
- Failure tests do not leave buttons or modifiers pressed.
- Concurrent action submissions cannot interleave.

### Phase 2: Desktop image MVP

Deliverables:

- [ ] Debian stable image
- [ ] s6-overlay service definitions
- [ ] Unprivileged desktop user
- [ ] Xvfb deterministic display profile
- [ ] D-Bus and AT-SPI startup
- [ ] XFCE session
- [ ] `xenoteerd` service
- [ ] x11vnc, websockify, and noVNC
- [ ] xdotool diagnostic package
- [ ] Fonts, locale, and clipboard basics
- [ ] Health and readiness endpoints
- [ ] Clean shutdown behavior

Acceptance gate:

- One documented container command boots the environment.
- A human can view and interact with XFCE through noVNC.
- The Rust input worker visibly moves the cursor with interpolation.
- Restart and shutdown leave no orphaned processes.
- The normal run command does not require privileged mode.

This is the first public vertical-demo milestone.

### Phase 3: Raw control plane

Deliverables:

- [ ] HTTP state endpoints
- [ ] WebSocket command and event channel
- [ ] Authentication token
- [ ] Exclusive controller lease
- [ ] Command deduplication
- [ ] Deadlines and safe cancellation
- [ ] Atomic action sequences
- [ ] Capability-gated application launch
- [ ] Mouse and keyboard APIs
- [ ] EWMH window search and manipulation
- [ ] `xenoteerctl` raw-control commands

Acceptance gate:

- A black-box client launches a terminal.
- It waits for and activates the terminal window.
- It smoothly moves, clicks, and types a command.
- It verifies the result and closes the window.
- Retrying a completed command ID does not repeat the physical action.

### Phase 4: Observation and synchronization

Deliverables:

- [ ] Full-screen, window, and region screenshots
- [ ] Pointer and input-state queries
- [ ] Window tree and stacking order
- [ ] Active and focused window queries
- [ ] Titles, classes, PIDs, geometry, and state
- [ ] CLIPBOARD and PRIMARY ownership and transfer
- [ ] State-based window waits
- [ ] Desktop and window event subscriptions
- [ ] Display generation identifiers
- [ ] Structured action traces

Acceptance gate:

- The Phase 3 workflow uses state-based waits rather than fixed sleeps.
- Clipboard contents survive according to X11 selection semantics while owned by
  Xenoteer.
- Screenshots and queried geometry use the same coordinate space.
- Every mutating action can return useful postcondition evidence.

### Phase 5: AT-SPI semantic automation

Deliverables:

- [ ] Accessibility connection and capability detection
- [ ] Tree snapshot
- [ ] Incremental event and cache processing
- [ ] Semantic selector engine
- [ ] Text, value, state, relation, and bounds queries
- [ ] Accessible action invocation
- [ ] Editable text support
- [ ] Semantic waits and subscriptions
- [ ] `element.invoke`
- [ ] `element.click` using physical interpolated input
- [ ] Stale-element detection and recovery guidance

Test applications:

- [ ] GTK
- [ ] Qt
- [ ] Chromium or Electron
- [ ] Firefox
- [ ] An application with incomplete or missing accessibility support

Acceptance gate:

- A client finds a button by semantic properties.
- It resolves current screen bounds.
- It physically moves to and clicks the button.
- It verifies the resulting accessibility or UI state event.
- Raw coordinate control remains usable when accessibility is unavailable.

The public API remains unstable until this phase is complete.

### Phase 6: Puppeteer-style SDK and developer experience

Deliverables:

- [ ] Stable protocol version 1 candidate
- [ ] Rust SDK
- [ ] TypeScript SDK
- [ ] Python SDK
- [ ] Session and desktop objects
- [ ] Mouse, keyboard, window, and accessibility objects
- [ ] Async wait primitives
- [ ] Structured errors and recovery hints
- [ ] Action tracing support
- [ ] Reconnect without automatic replay of mutating actions
- [ ] Examples and API documentation
- [ ] Shared conformance suite

Target usage:

```rust
let desktop = client.desktop().await?;
let save = desktop.find(role("button").name("Save")).await?;
save.click().await?;
desktop.wait_for(text("Saved")).await?;
```

Acceptance gate:

- The same black-box conformance workflows pass through every supported SDK.
- Mutating operations are never silently replayed after reconnect.
- Protocol version negotiation and unsupported capabilities fail clearly.

### Phase 7: Hardening and first release

Deliverables:

- [ ] Minimum-privilege runtime review
- [ ] Authentication and origin policy
- [ ] Capability-gated process and filesystem operations
- [ ] Resource limits and action timeouts
- [ ] Secret redaction
- [ ] Audit logs
- [ ] Crash and stuck-input recovery
- [ ] Reproducible pinned image builds
- [ ] Multi-architecture images where dependencies permit
- [ ] SBOM and vulnerability scanning
- [ ] Dependency-license validation
- [ ] Upgrade and compatibility policy
- [ ] Operations and troubleshooting documentation
- [ ] End-to-end smoke-test image

Acceptance gate:

- A clean machine can pull the documented image and pass the release smoke test.
- The build is reproducible from the tagged source and pinned inputs.
- No known action can leave the session permanently input-locked.
- Security-sensitive capabilities are disabled unless explicitly enabled.

## 9. Cross-Phase Test Strategy

### Input correctness

- Event order and timing
- Exact endpoints
- Pointer confinement and coordinate clamping
- Hover-sensitive paths
- Drag thresholds
- Double-click timing
- Stuck-key and stuck-button recovery
- Concurrent request serialization
- Cancellation at safe boundaries

### Desktop compatibility

- GTK
- Qt
- Electron/Chromium
- Firefox
- Java/Swing when practical
- Terminal emulators
- Applications with custom canvas rendering
- Applications with no useful accessibility tree

### Reliability

- Repeated container startup and shutdown
- Daemon restart while the desktop remains alive
- Viewer reconnect
- Client reconnect
- Window-manager restart
- D-Bus or AT-SPI degradation
- Application crash during an action
- Long-running sessions
- Low-memory and full-disk behavior

### API safety

- Duplicate command IDs
- Expired deadlines
- Lease loss
- Unauthorized clients
- Oversized payloads
- Invalid coordinates and window IDs
- Stale element references
- Destructive capability denial

## 10. Explicitly Deferred Beyond 1.0

- Wayland
- GPU acceleration
- Audio streaming
- Multiple monitors
- Dynamic in-session display resizing
- Touch and multitouch
- XI2 smooth touchpad scrolling
- Unseeded humanized cursor movement
- Multi-container fleet scheduling
- Kubernetes operators
- Hosted control-plane product

These features must not distort the initial API or delay the reliable single
X11 desktop milestone.

## 11. Release Definition

The first release is complete when a user can:

1. Start one unprivileged Xenoteer container with a documented command.
2. Watch its XFCE desktop through a browser.
3. Connect through a supported SDK.
4. Launch and identify a desktop application.
5. Inspect its accessible UI when available.
6. Smoothly move, click, drag, scroll, and type using physical XTEST input.
7. Capture screenshots and clipboard state.
8. Wait for observable postconditions without arbitrary sleeps.
9. Recover cleanly from client disconnects and failed actions.

## 12. Technical References

- XTEST protocol: <https://www.x.org/releases/X11R7.6/doc/xextproto/xtest.html>
- xdotool and libxdo: <https://github.com/jordansissel/xdotool>
- x11rb: <https://docs.rs/x11rb/latest/x11rb/>
- Rust AT-SPI: <https://docs.rs/atspi/latest/atspi/>
- EWMH: <https://specifications.freedesktop.org/wm/latest-single/>
- X11 clipboard conventions: <https://specifications.freedesktop.org/clipboard/latest>
- s6-overlay: <https://github.com/just-containers/s6-overlay>
