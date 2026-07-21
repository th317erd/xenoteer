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
  available. cargo-nextest remains optional and is not installed.

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
- The actor owns a second, non-XTEST XCB connection only for xkbcommon model
  construction/refresh. Mapping events are consumed on the actor's x11rb
  connection before deterministic model rebuild.
- Core QueryPointer residual-button evidence is complete only for buttons 1–5;
  higher-button verification remains explicitly partial until an XI2 query is
  implemented. Keyboard cleanup uses QueryKeymap evidence.

## Working conventions

- Phase commits stay local until all achievable phases are complete; do not push
  without user confirmation, per the implementation workflow.
- Every phase adds tests and preserves all earlier gates.
- Record environmental verification gaps as gaps, never as successful gates.
