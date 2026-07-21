# Dependency-ordered implementation phases

## 1. Execution rules

Implement phases in order. A later phase may prepare schemas/fixtures in
parallel, but it must not declare its feature usable before every dependency gate
passes. Each phase ends with:

- a coherent commit/merge boundary;
- all new tests green and prior gates preserved;
- updated plan/schema/config/docs for any discovered constraint;
- dependency/license/advisory review for additions;
- demo script using only interfaces intended to survive the next phase.

No phase is complete with a mocked success path when its purpose is backend
proof. Unsupported items remain explicit capability results, never implicit
stubs.

## 2. Phase dependency map

```text
0 Foundations and risk spikes
  -> 1 Native physical input kernel
      -> 2 Deterministic desktop container
          -> 3 Raw control plane and process lifecycle
              -> 4 Observation, windows, clipboard, capture, viewer
                  -> 5 AT-SPI semantic automation
                      -> 6 SDKs and CLI
                          -> 7 Hardening and release
```

Some fixture work spans phases, but the public product remains internal/dev until
phase 3, and release candidate status begins only in phase 7.

## 3. Phase 0 — foundations and risk spikes

### Objective

Create the repository/workspace/build discipline and retire the highest-risk
unknowns with small executable proofs before broad implementation.

### Deliverables

#### Repository and Rust workspace

- Workspace/crates/module skeleton from [09](09-rust-code-design.md).
- Pinned `rust-toolchain.toml`, Cargo.lock, resolver/profile/lint policy.
- CI for fmt, Clippy, unit/doc tests, schema diff, cargo-deny.
- Protocol v1 IDs, geometry, envelope, capability, result/problem skeletons.
- JSON schema generation with golden diff.
- Config load/merge/unknown-field/redacted Debug skeleton.
- `tracing` JSON startup and request span skeleton.

#### Build/runtime skeleton

- Multi-stage Debian Dockerfile with pinned base digest.
- s6-overlay v3 verified installation and one trivial readiness service.
- Dependency/download lock format.
- Image labels, non-root user, runtime directories, `/dev/shm` validation.
- Minimal license/SBOM pipeline that inventories all current contents and fails
  on unknown provenance from its first revision.

#### Risk spikes (kept as tests/examples or discarded after findings recorded)

1. x11rb `RustConnection` to Xvfb: core round trip and extension inventory.
2. XTEST event delivered to `x11-event-recorder` and QueryPointer barrier.
3. Determine observation/clipboard event-loop integration: x11rb connection FD
   with Tokio AsyncFd/event-loop feature; if incompatible, prove wake-FD poll
   thread. Record chosen mechanism in [09].
4. libxkbcommon model from the X server and one keycode mapping.
5. Rust atspi/zbus Tokio connection to a minimal GTK fixture.
6. X GetImage decode of color-bar fixture (24 depth/32 bpp case).
7. x11vnc loopback view-only -> websockify -> pinned noVNC smoke.
8. Chromium and QtWebEngine start under proposed non-root/container sandbox with
   2 GiB `/dev/shm`; record host/runtime requirements.

#### Test fixtures

- Initial X11 event recorder.
- Minimal GTK button/entry fixture.
- Color-bar window.
- Harness for isolated Xvfb/Xauthority/display.

### Decisions closed by phase

- Exact stable direct dependency versions/features.
- Observation/clipboard connection integration mechanism.
- Pixel internal format and PNG/resize crates.
- Chromium sandbox profile compatible with Docker defaults.
- noVNC/websockify distribution mechanism and pinned versions.
- Actual s6 service definition conventions.

### Exit gate

- Fresh clone runs full phase tests with documented tools.
- No unreviewed unsafe in first-party code.
- Each spike has executable evidence and decision update; no unresolved blocker
  to phases 1–3.
- Container PID 1/readiness/shutdown works and daemon skeleton runs non-root.
- Cargo/image dependency license allowlist passes.

### Non-goals

No stable control API, full desktop, semantic selector, or polished viewer.

## 4. Phase 1 — native physical input kernel

### Objective

Prove correct, deterministic, serialized XTEST input and cleanup independently
of the final container/API.

### Work packages

#### Core algorithms/state

- Checked root geometry/coordinate types.
- Interpolation automatic duration, linear/smooth/waypoint generation, exact
  delay distribution, duplicate compaction.
- InputState/effect journal/reset state machines.
- Button mapping and key resolution domain types.
- Property tests from [03](03-input-engine.md).

#### X11 input actor

- Dedicated connection/thread/typed handle/queue reserved control capacity.
- XTEST version/probe, core QueryPointer barrier, protocol error conversion.
- Absolute/relative instant and interpolated move.
- Click/count/down/up/drag/discrete scroll.
- Pressed key/button tracking and reverse cleanup.
- Actor health/probe/panic join/poison behavior.

#### Keyboard

- XKB model/server mapping notifications.
- Named keys, press/down/up/chord/sequence.
- Current-layout physical Unicode.
- Explicit temporary unused-keycode mapping and restoration, behind advanced
  policy; injected restoration failure poisons actor.

#### Test/diagnostic surface

- Internal harness/CLI invokes typed actor, not public server protocol yet.
- Event recorder expanded for all pointer/key/button paths and failure modes.
- xdotool oracle comparisons documented where semantics align.

### Exit gate

- Exact endpoint and delay-sum properties pass across generated domain.
- Event recorder proves intermediate hover motion, instant mode, click timing,
  drag held state, scroll notches, key/chord order.
- Two concurrent submitters cannot interleave actions.
- Every injected failure after press attempts release; residual state/poison is
  correct.
- XTEST connection delay does not block an independent observation connection.
- No daemon path shells out to xdotool.

### Non-goals

No network API, clipboard text strategy, window activation, AT-SPI, or final
desktop image.

## 5. Phase 2 — deterministic desktop container

### Objective

Turn the build/runtime skeleton into the supported Xvfb/XFCE desktop with
truthful service ordering, accessibility activation, viewer services, and
browser/toolkit profiles.

### Work packages

#### Image and s6 graph

- Exact Debian package groups and package/source lock.
- s6-rc graph: runtime paths, Xauthority, Xvfb, session D-Bus, AT-SPI, XFCE,
  daemon, x11vnc, websockify.
- Readiness notification/probes and critical/optional restart policies.
- Graceful reverse-order shutdown.

#### Desktop profiles

- `standard` and `bare` XFCE profiles.
- Session save/cache, agents, autostart, compositor, blanker/power/locker,
  notifications, panel, wallpaper, focus policy all deterministic.
- Locale/timezone/layout/fonts/theme fixed and inventoried.
- Post-readiness process allowlist.

#### Application profiles/fixtures

- Full GTK/Qt fixtures and window variants.
- Chromium/Firefox profiles with first-run/telemetry/session restore disabled and
  accessibility forced.
- QtWebEngine/Electron smoke fixture.
- Browser sandbox doctor and shared-memory warning.

#### Runtime profiles

- Development Compose example.
- Hardened read-only-root/tmpfs/cap-drop/no-new-privileges candidate with
  documented browser compatibility.
- Listener/auth/Xauthority negative tests.

### Exit gate

- One command boots to Ready, twice on clean home and twice on persistent test
  home without resurrecting apps.
- Required X/D-Bus/AT-SPI/window manager/input/capture probes pass.
- No unexpected process/listener, unauthenticated X access, compositor, blanker,
  or saved session.
- GTK/Qt/Chromium/Firefox fixture windows appear; required accessibility roots
  exist where supported.
- 30-minute idle and graceful/forced shutdown tests pass; no zombie/stuck input.
- Hardened candidate boots or has exact recorded exceptions with mitigation and
  phase-7 closure task; no privileged mode or browser `--no-sandbox`.

### Non-goals

No stable external API. noVNC smoke can be direct local development routing but
must already be server-side view-only.

## 6. Phase 3 — raw control plane and process lifecycle

### Objective

Expose secure, versioned, retry-safe raw input and managed application lifecycle
through HTTP/WebSocket; establish the coordinator/lease/ledger/event foundation.

### Work packages

#### Core coordination

- Coordinator actor, lease state/TTL/renew/revoke/reset.
- Command ledger canonical hash/dedupe/in-progress/terminal/TTL.
- Deadline/cancel/effect result semantics.
- Desktop supervisor generations/capability state.
- Bounded event hub with sequence/replay/resync.

#### API transport

- Auth token provider/capabilities/middleware.
- `/livez`, `/readyz`, `/v1/status`, capabilities.
- Lease/command submit/get/wait/cancel REST.
- WS hello/welcome/command/result/events/heartbeat/close.
- RFC 9457 problem conversion and rate/body/concurrency limits.
- OpenAPI/JSON schema examples.

#### Process manager

- Registered application profiles and argv/env/cwd validation.
- Process group, reference with `/proc` start time, output limits.
- Exit event, SIGTERM/grace/SIGKILL/reap.
- No shell/arbitrary PID operations.

#### Raw feature wiring

- Mouse/keyboard/input reset commands from phase 1.
- Launch/terminate/process status.
- Harmless/raw status events and action traces.
- Minimal Rust SDK transport used by black-box tests (not yet polished release).

### Exit gate

- Auth and authorization negative matrix passes.
- Concurrent duplicate command executes exactly once; changed body conflicts.
- Disconnect at before/after acceptance/effect/result resolves according to
  ledger; no auto replay.
- Lease conflict/expiry/disconnect grace/reset verified.
- Queue/backpressure/oversize/slow WS cannot exhaust daemon.
- Managed child fork/output/hang/terminate tests leave zero zombies.
- Full raw workflow works black-box: boot, auth, lease, launch recorder, smooth
  input, observe command result, terminate, shutdown.

### Non-goals

No stable window/element handles, screenshot artifacts, clipboard, or final SDK
ergonomics.

## 7. Phase 4 — observation, windows, clipboard, capture, viewer

### Objective

Make the desktop observable and targetable without polling sleeps, complete
X11-native "xdotool whistles," and provide secure human viewing.

### Work packages

#### Window observation/control

- Observation actor, atom/property parsers, initial reconciliation/events.
- WindowRef tombstones/identity validation.
- Snapshot/selectors/ordering/pagination/waits.
- EWMH activation/close/state/move/resize with observed results.
- Window-process correlation and geometry spaces.

#### Clipboard/text

- ClipboardActor hidden window/event loop.
- CLIPBOARD/PRIMARY get/set/clear, required targets, MULTIPLE, INCR.
- Clipboard paste coordination/preservation/value-copy restoration.
- `text.insert` strategy engine integrating physical/clipboard paths (AT-SPI path
  waits for phase 5).

#### Capture/events/artifacts

- Core root/window-visible/window-drawable GetImage.
- Pixel decode, PNG/raw, cursor composition, limits.
- DAMAGE hints/coalescing.
- Artifact filesystem store/authorization/retention.
- MIT-SHM optimization only after parity gate.

#### Viewer

- x11vnc locked server-side view-only/loopback.
- websockify loopback and authenticated viewer gateway/tickets.
- Pinned noVNC static UI, CSP/origin/input/clipboard/file-transfer disabled.
- Viewer backend status/restart/conformance boundary.

#### Public API

- Snapshot/query/window/clipboard/capture/artifact/viewer routes/commands/events.
- Raw physical click can target strict window-relative coordinates with activation
  and near-execution revalidation.

### Exit gate

- Window XID reuse, ambiguity, activation refusal, geometry constraints, and
  race-free wait tests pass across fixture matrix.
- Clipboard direct/INCR/get/set/paste/restore cases pass GTK/Qt/browsers.
- Capture correctness/limits/cursor/overlap/minimize and SHM parity pass.
- Viewer is externally accessible only through authenticated WSS gateway and
  cannot deliver pointer/key/clipboard events to recorder.
- Viewer crash degrades viewing without breaking input; Xvfb crash invalidates
  all generations and stops cleanly.
- Event flood/slow client produces resync, never blocks X actor or command result.

### Non-goals

No semantic element selectors/actions, OCR, high-resolution XI2 scroll, or human
takeover.

## 8. Phase 5 — AT-SPI semantic automation

### Objective

Add bounded semantic discovery/actions/waits and carefully bridge element
geometry to physical input.

### Work packages

- AtspiActor connection/generation/reconnect.
- Bulk Cache GetItems plus old Qt compatibility/lazy fallback.
- Bounded cache/reconcile/events/rebuild.
- ElementRef/snapshot/selector/query/waits/pagination policy.
- Application/window correlation evidence/confidence.
- Semantic invoke/focus/value/selection/text/scroll with postconditions.
- Physical `element.click` geometry/scroll/activation/revalidation/occlusion.
- `text.insert auto` completes AT-SPI strategy path.
- Protected-field redaction and semantic read/write capabilities.
- GTK/Qt/browser/custom/large/malformed fixture matrix.

### Exit gate

- Standard controls discover/action correctly in supported fixtures.
- Semantic invoke and physical click remain distinct in schema/trace/effect.
- Stale refs after removal/app restart/browser navigation/bus reconnect never
  retarget.
- Large/malformed tree/event flood stays within budgets and recovers/resyncs.
- Missing/incomplete accessibility returns honest capability/error while physical
  paths remain ready.
- Geometry revalidation prevents queued click on moved/replaced element.
- AT-SPI editable text/value/selection verifies outcome and protects secrets.

### Non-goals

No guarantee for custom canvas widgets, CSS selectors, OCR, or browser DevTools.

## 9. Phase 6 — SDKs and CLI

### Objective

Freeze the v1 developer experience, publish the Rust reference SDK and CLI, then
TypeScript and Python with one conformance corpus.

### Work packages

#### Protocol stabilization

- Review every command/result/error/event/default/capability for names and
  forward compatibility.
- Freeze v1.0 schema and compatibility test vectors.
- Server/SDK protocol range negotiation and cross-version fixtures.

#### Rust SDK

- Client/transport/reconnect/heartbeat.
- CommandHandle/dedupe lookup/unknown-after-restart.
- Lease scoped helper.
- Mouse/keyboard/windows/elements/clipboard/apps/capture/viewer/events objects.
- Error/redaction and documentation examples.
- Apache package-content/license enforcement.

#### CLI

- Full command tree, human/JSON/binary output discipline, exit codes.
- `doctor` including optional input/browser/viewer tests.
- Safe token sources and selector/command-ID scripting.

#### TypeScript/Python

- Generated wire types plus idiomatic async wrappers.
- Connection/reconnect/cancellation semantics.
- Same examples and language-neutral conformance cases.
- Packaging/provenance/license checks.

### Exit gate

- Every public quick-start/example passes clean against release-candidate image.
- All SDKs pass shared JSON/reconnect/stale/effect/event/redaction conformance.
- Newest/oldest supported protocol cross-tests pass.
- Package inspection contains no BSL server implementation.
- Docs lead with waits/postconditions and safe retry, not sleeps/fire-and-forget.
- CLI doctor diagnoses all documented host/runtime gotchas.

### Non-goals

No synchronous SDK facade, browser-control SDK with long-lived API token, or
unversioned convenience endpoint.

## 10. Phase 7 — hardening and first production release

### Objective

Close security/operational gaps, prove resource behavior, produce verifiable
artifacts, and release without hidden deployment questions.

### Work packages

#### Runtime/security

- Hardened read-only-root/tmpfs/cap-drop/seccomp/LSM profile finalized.
- Browser sandbox status on supported hosts; no `--no-sandbox`.
- Listener/auth/origin/proxy/token rotation/object authorization tests.
- Path/process/output/regex/parser/DoS/redaction fault/fuzz matrix.
- Secret canary scan and core dump/log/artifact policy.

#### Reliability/performance

- Full failure injection from [13](13-testing-and-quality.md).
- Queue/budget/load tests and documented reference performance.
- 24-hour desktop/browser/viewer/action soak; zero leak/zombie trend.
- Graceful/forced shutdown at every effect stage.
- x11vnc risk review and disable/replace switch tested.

#### Supply chain/legal

- Base/dependency locks and image content allowlist.
- Cargo/container advisories triaged with expiring waivers only.
- SBOM/provenance/cosign signature from protected workflow.
- Third-party notices and corresponding-source fulfillment tied to digest.
- BSL server/Apache SDK/schema package boundaries audited.

#### Operations/docs

- Dev and hardened Compose examples.
- Auth/TLS/proxy, `/dev/shm`, resources, persistence/reset, upgrade/rollback,
  metrics/alerts, incident guidance.
- Clean-machine quick start by digest.
- Release notes with known limitations/deferred scope.

### Exit gate

All release quality criteria in [13](13-testing-and-quality.md) and artifact/
operations criteria in [14](14-build-release-operations.md) pass. Every critical
behavior has a recorded implementation decision.

## 11. Post-1.0 candidate phases (not release-one commitments)

Order based on measured demand/risk:

1. Replace/augment x11vnc with maintained native viewer/streamer.
2. Explicit human takeover transaction (still no collaborative input).
3. Multi-workspace and fixed multi-monitor profiles.
4. OCR/image matching observation adapter.
5. High-resolution XI2 scroll/touch/gesture.
6. Wayland backend under a new capability/compatibility boundary.
7. Fleet/session orchestration in a separate service.

Each begins with an updated product decision/threat model; none is squeezed into
a patch release as an accidental flag.

## 12. Phase ownership handoff template

At phase start, assign named owners for:

- domain/protocol;
- backend/runtime;
- fixtures/tests;
- security/license;
- docs/SDK where applicable.

At phase end, handoff records:

- exact commit/image digest;
- demo and test commands;
- configuration/default changes;
- known limitations with issue/owner/target phase;
- dependencies/licenses/advisories added;
- performance/resource measurements;
- migrations/schema changes;
- next-phase prerequisites verified.

## 13. Primary references

Subsystem-specific protocol and upstream sources are maintained in the linked
plans. The sources governing execution mechanics across phases are:

- [Cargo test organization](https://doc.rust-lang.org/cargo/commands/cargo-test.html)
- [Cargo target layout](https://doc.rust-lang.org/cargo/reference/cargo-targets.html)
- [Docker multi-stage builds](https://docs.docker.com/build/building/multi-stage/)
- [Docker build attestations](https://docs.docker.com/build/metadata/attestations/)
- [s6-overlay service supervision](https://github.com/just-containers/s6-overlay)
- [XTEST protocol](https://www.x.org/releases/X11R7.6/doc/xextproto/xtest.html)
- [AT-SPI architecture](https://gnome.pages.gitlab.gnome.org/at-spi2-core/devel-docs/architecture.html)
