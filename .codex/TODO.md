# Xenoteer implementation progress

Plan: `plans/15-phased-implementation.md`

## Phase 0 — foundations and risk spikes

- [x] Rust workspace, policies, protocol/core/server/CLI skeletons
- [x] Schema generation and golden validation
- [x] Typed configuration merge/validation/redaction
- [x] CI, formatting, lint, tests, dependency/license policy
- [x] Multi-stage image, s6 skeleton, user/runtime paths, readiness
- [x] X11/XTEST/event-loop/XKB/AT-SPI/capture/viewer/browser spikes
- [x] Phase 0 full verification and handoff

## Phase 1 — native physical input kernel

- [ ] Core geometry/interpolation/state/effect algorithms and property tests
- [ ] Dedicated XTEST input actor and cleanup/poison behavior
- [ ] Keyboard mapping, physical text, chord/sequence behavior
- [ ] Fixture/diagnostic conformance and phase verification

## Phase 2 — deterministic desktop container

- [ ] Complete s6/Xvfb/D-Bus/AT-SPI/XFCE/viewer service graph
- [ ] Deterministic standard/bare desktop profiles
- [ ] GTK/Qt/browser application profiles and fixtures
- [ ] Development/hardened runtime profiles and phase verification

## Phase 3 — raw control plane and process lifecycle

- [ ] Coordinator, lease, ledger, deadlines, generation, event hub
- [ ] Authenticated HTTP/WebSocket protocol and limits
- [ ] Managed application lifecycle
- [ ] Raw input integration and black-box verification

## Phase 4 — observation, windows, clipboard, capture, viewer

- [ ] X11 observation/window identity, query, control, waits
- [ ] Clipboard and text strategy engine
- [ ] Capture, DAMAGE, artifacts, and viewer gateway
- [ ] Public APIs and phase verification

## Phase 5 — AT-SPI semantic automation

- [ ] AT-SPI actor/cache/reconciliation
- [ ] Elements/selectors/waits/correlation
- [ ] Semantic and physical element actions
- [ ] Toolkit/browser/malformed-tree conformance

## Phase 6 — SDKs and CLI

- [ ] Freeze v1 protocol and compatibility fixtures
- [ ] Rust SDK and full CLI
- [ ] TypeScript SDK
- [ ] Python SDK
- [ ] Cross-language conformance and packaging verification

## Phase 7 — hardening and first release

- [ ] Runtime/security hardening and fault/fuzz matrix
- [ ] Reliability, performance, and soak verification
- [ ] Locks, SBOM, provenance, signatures, notices, source fulfillment
- [ ] Operational documentation and release gates

## Current constraints

- Docker Engine 29.1.3 and Docker Compose 5.3.1 are installed and verified. The
  current shell requires `sudo docker` until group membership is refreshed.
- Rust/Cargo 1.97.1, Xvfb, D-Bus, XKB, AT-SPI, GTK, cargo-deny, cargo-audit,
  shellcheck, noVNC, websockify, and the viewer/browser test dependencies are
  installed. cargo-nextest is not installed and is not a Phase 0 requirement.
- Chromium's supported sandbox path requires the pinned, narrow seccomp profile,
  a private `/dev/shm` of at least 4 GiB, and the documented seven-capability
  allowlist in the hardened s6-overlay runtime. `KILL` is retained solely so
  root PID 1 can stop supervised UID 1000 payloads cleanly.
