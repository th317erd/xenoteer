# Xenoteer implementation details

## Baseline

- Repository: `th317erd/xenoteer`, branch `main`.
- Planning baseline commit: `5327508`.
- Normative phase plan: `plans/15-phased-implementation.md`.
- Server/runtime license: repository BSL 1.1 parameters.
- Protocol and separately packaged SDKs: Apache-2.0 when their directories are
  created.

## Closed architecture

- One Debian/XFCE/Xvfb desktop per release-one container.
- Rust workspace with protocol, core, X11, AT-SPI, server, SDK, and CLI crates.
- Exactly one dedicated XTEST input actor/connection; observation and clipboard
  own independent X connections.
- Axum/Tokio JSON HTTP/WebSocket control plane.
- TigerVNC `X0tigervnc`/websockify/noVNC is server-side view-only and replaceable.
- All references are desktop-generation-bound; command effects use a bounded
  deduplication ledger and explicit effect stages.

## Environment provisioned and verified 2026-07-20

- Rust/Cargo 1.97.1 are available.
- Docker Engine 29.1.3 and Docker Compose 5.3.1 are available; use `sudo docker`
  until the login session refreshes its Docker-group membership.
- Xvfb, `dbus-run-session`, XKB, AT-SPI, GTK, cargo-deny, cargo-audit,
  shellcheck, noVNC, websockify, and browser/viewer spike dependencies are
  available. `xdotool` is installed for fixture/oracle use only; production
  paths may not invoke it. cargo-nextest remains optional and is not installed.

## Phase 0 evidence

- Portable workspace format, clippy, unit/integration/doc tests, rustdoc, schema
  drift, cargo-deny, and source/license policy gates pass.
- Live Xvfb XTEST, XKB, capture, AT-SPI, and concurrent isolated-display
  harnesses pass.
- The pinned production image passes PID 1, non-root ownership, authenticated
  X11, health/readiness, manifests/SBOM, graceful shutdown, hardened profile,
  critical-service failure, and missing-secret gates.
- The final Phase 0 production image
  `sha256:b9fbb008c802eb8cbe3254ce2d2b3b9694c40d59b24b21fefbf8dfaa5fb16b44`
  independently passes the full lifecycle matrix: requested stops before and
  after readiness exit zero; unexpected Xvfb or daemon exits halt without
  respawn and return nonzero in normal and hardened profiles; deleting only
  `CAP_KILL` from the hardened profile reproduces Docker's forced exit 137.
- Chromium and QtWebEngine pass DOM/render and sandbox proofs with the pinned
  Docker baseline seccomp profile extended only for `clone`, `setns`, and
  `unshare`.
- TigerVNC `X0tigervnc` behind websockify/noVNC passes real WebSocket/RFB/render
  tests while server-side keyboard, pointer, and clipboard input remain denied.
- Browser and viewer spike images are explicitly marked non-distributable.

## Phase 1 implementation seams closed 2026-07-20

- The normal input queue is bounded at 256 and has a distinct capacity-one
  control channel; shutdown dominates a coalesced reset request.
- A whole waypoint action is bounded at 10,000 ms and 4,096 emitted XTEST
  events. Instant motion accepts only omitted/zero duration. Zero-distance
  motion has effective duration zero unless diagnostic `emit_noop` is explicit.
- Every XTEST `VoidCookie` is retained and consumed with `VoidCookie::check`;
  QueryPointer is an ordering/observation barrier, not void-request error proof.
- Submitted press/release effects remain provisional until checked requests and
  the observation barrier succeed, enabling conservative cleanup on uncertainty.
- The actor owns a second, non-XTEST XCB connection for xkbcommon model
  construction, state refresh, and XKB/core mapping notifications. Keyboard
  preflight establishes a reply-producing synchronization point, drains that
  connection, rebuilds a dirty model, resolves under the new generation, and
  brackets serialization with another generation check; the dedicated XTEST
  connection remains the sole injector.
- Core QueryPointer residual-button evidence is complete only for buttons 1–5;
  higher-button verification remains explicitly partial until an XI2 query is
  implemented. Keyboard cleanup uses QueryKeymap evidence.
- The 4,096-event ceiling applies to the complete planned compound XTEST action,
  including motion plus button/key events. Mandatory recovery releases are not
  admission-budgeted; the command-scoped effect journal has a separate 4,608
  record ceiling, reserving room for every core keycode and physical button.
- Automatic interpolation consumes a checked `MotionPolicy` copied from the
  validated input configuration; defaults remain 1,200 px/s, 80--650 ms, and
  60 Hz, but non-default configured values are not silently ignored.
- Key chords contain 1--16 distinct resolved physical keys. Modifier presses
  are stably partitioned ahead of non-modifiers in caller order, and the entire
  press order is released in reverse.
- Effect journals are action-scoped rather than daemon-lifetime-scoped. A
  terminal connection/panic transition retains provisional evidence and owned
  input conservatively while abandoning only the batch bookkeeping required to
  close the failed command; poisoned health never returns to ordinary service.
- Motion plans preserve normalized segment ranges instead of flattening
  waypoints into an indistinguishable sample stream. Move/click pre-movement
  and drag execution can therefore place checked-request barriers and evaluate
  cancellation/deadlines only at documented host-controlled segment
  boundaries; duplicate waypoints collapse and an explicit final no-op remains
  a distinct boundary.
- The independently cleared pointer actor uses one bounded FIFO plus a distinct
  coalesced control lane, checked XTEST requests, deferred ownership
  confirmation from QueryPointer masks, and an explicit poisoned-reset batch
  abandonment seam. Its final focused evidence is 29 actor/backend tests plus
  eight core state tests; strict all-feature clippy, rustdoc, formatting, and
  diff gates pass.
- Keyboard resolution distinguishes physical-key intent from exact-text intent.
  Only freshly observed, exclusively actor-owned depressed modifiers may be
  discounted for a physical key; exact text never discounts them. XKB event
  subscription precedes authoritative model construction and includes behavior
  changes. Bindings and unused-keycode reservations are model-bound opaque
  tokens validated through synchronized server round trips. Model diagnostics
  retain strict `_XKB_RULES_NAMES` metadata and an FNV-1a-64 fingerprint of the
  complete serialized keymap.
- The independently cleared XKB model passes 10 portable and 17 native tests.
  Two additional authenticated live-Xvfb tests prove owned/external modifier
  adjudication, opaque token tamper resistance, held-binding current/stale
  behavior, real US-to-US/German mapping notifications, fingerprint changes,
  current-group selection, and German AltGr resolution.
- `ExtendedTemporaryMapping` is a low-level Phase-1 mechanism, not authority to
  mutate a shared desktop keymap. Phase 3 must reject it unless the coordinator
  proves an exclusive controller lease and the server-side VNC path is
  view-only; this global policy cannot be inferred by the input actor itself.
- The final actor has a wire-side sent-press ledger independent of logical
  state. Reset, shutdown, and panic cleanup therefore release and independently
  prove keys/buttons whose press reached the backend before a panic, connection
  error, or failed state mutation; a temporary-mapped key is released before
  its original mapping is restored.
- Scalar-bearing actions structurally expose only aggregate effect and cleanup
  evidence. Named/raw actions retain detailed physical diagnostics. Pending
  temporary-map restoration also redacts the keymap fingerprint so public
  errors, health snapshots, and `Debug` output cannot dictionary-encode text.
- Xvfb/XKB canonicalizes a one-symbol temporary core mapping by duplicating
  group one into slot two. Installation admits only the exact-width canonical
  row `[U, NoSymbol, U, NoSymbol, ...]`; malformed populated slots or row shape
  fail closed, while restoration must match the complete original row exactly.
- The Phase-1 recorder and no-skip Xvfb harness prove pointer interpolation,
  click/drag/scroll, cancellation cleanup, independent observation during an
  XTEST delay, named/scalar/chord/sequence/current-layout input, and temporary
  U+2603 install -> key press/release -> exact restore ordering. The final
  stable candidate passed workspace default/all-feature all-target tests,
  strict Clippy, warning-denied rustdoc, formatting, ShellCheck, cargo-deny, and
  independent adversarial review.

## Phase 2 implementation bindings prepared 2026-07-20

- The locked Debian stable snapshot is Debian 13/trixie. Install explicit XFCE
  components rather than the `xfce4` metapackage, and generate an exact
  version/architecture/package SHA/source lock from the snapshot.
- Supervise one session `dbus-daemon` and invoke `xfce4-session --disable-tcp`
  directly. `dbus-x11`, `dbus-launch`, and `startxfce4` are excluded because
  they can create competing buses, wrappers, and desktop agents.
- Immutable bare/standard profile assets populate ephemeral XDG config/cache
  paths under `/run/user/1000` on every boot; persistent home contents are not
  deleted or allowed to resurrect saved XFCE sessions implicitly.
- Session D-Bus, AT-SPI, and XFCE are critical release-one services. Viewer
  services are optional unless configured required and therefore cannot block
  the default s6 startup-readiness transaction.
- noVNC static assets come from a verified, locked Debian package extraction
  stage; the final image does not install noVNC's unnecessary Node.js runtime.
  `X0tigervnc` and websockify remain loopback-only and server-side view-only.
- Phase 2 daemon readiness proves X geometry/extensions, EWMH/workspace,
  compositor absence, an input-actor pointer probe, one-pixel capture, the
  session bus, AT-SPI registry, and optional viewer evidence before `/readyz`
  can claim readiness.

## Working conventions

- Phase commits stay local until all achievable phases are complete; do not push
  without user confirmation, per the implementation workflow.
- Every phase adds tests and preserves all earlier gates.
- Record environmental verification gaps as gaps, never as successful gates.
