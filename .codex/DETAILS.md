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
- The user has authorized the eventual verified Phase-6 milestone commit and
  push. Do not commit or push partial closure: the exact immutable-image gate
  and overall Phase-6 closure remain open.
- Every phase adds tests and preserves all earlier gates.
- Record environmental verification gaps as gaps, never as successful gates.
- Keep Rust builds at a maximum of two jobs and tests at a maximum of two test
  threads. Run CPU/I/O-heavy commands under `nice -n 15 ionice -c 3` with an
  explicit timeout so Xenoteer work does not starve other host processes.
- Parallel lanes must additionally serialize heavy Cargo build/test/Clippy/doc
  commands with `flock /tmp/codex/xenoteer-heavy-build.lock`; otherwise three
  independent `--jobs 2` checks would still become an accidental six-job build.
- Phase-6 conformance scenario/redaction cases must be concrete machine
  fixtures. An adapter may dispatch by the fixed operation type, but it must
  exercise public protocol/SDK behavior from the supplied values; it may not
  derive results from case IDs, narrative action labels, or expected assertion
  names. Mutation tests must prove a wrong fixture or wrong runtime outcome
  fails.
- The final concrete Phase-6 v1 corpus contains 8 suites/73 cases with SHA-256
  `6cc98e72e1de6591cce2d0661f4fc3ea508535d310a40746aa3ad8bd1e61e7fc`.
  The official Rust, TypeScript, and Python adapters each pass all 73 cases with
  zero skips and pin this exact protocol/corpus identity. Mutation tests prove
  incorrect fixtures and runtime outcomes fail.
- Final Phase-6 non-container verification is green: the full Rust workspace;
  Rustfmt, Clippy, Rustdoc, schema generation/check, API documentation, and the
  Rust public-package verifier; TypeScript 53/53 tests, conformance, and package
  inspection; Python 41/41 tests, Ruff, mypy, wheel/sdist inspection, and the
  deterministic event-completion barrier; the 13/13 CI-contract tests; and the
  static mirror, first-party license, and cargo-deny gates.
- Phase-6 public quick-start qualification must install the staged crate/npm/
  wheel artifacts rather than importing source trees, run every language
  against the same immutable release-candidate image ID, cover bounded
  invalid-auth/failure cleanup as well as success, and reject daemon binary
  overrides.
- The first exact Phase-6 candidates were production
  `sha256:723a53474f3ba61ed00695f7355a85a559b4bf4e393bb1f6fd6adc2dc7f06158`
  and fixture
  `sha256:4f094fc2d1c13bdb646ce15f679718d0b2efcbee3b75aead8f91d1776d6e0d09`
  from clean source commit `99ae21bc8aa78f2da73c437a55756af6bed77853`.
  All six serial Docker live gates passed. The final staged-package gate then
  reproduced a real cross-language example bug: each example overrode the
  SDK's 35-second request default with 5 seconds while issuing 10-30-second
  server long polls. The Rust crate passed status and scoped launch, then
  failed its first window wait with `SDK request timed out`. No package digest
  was accepted and both images must be rebuilt after the fail-first fix so
  their source identity remains exact.
- The first deadline-fix review confirmed that the implementations use a
  35-second transport request deadline around at most 30-second server long
  polls, but rejected the initial regression as incomplete: it counted current
  timeout expressions and could miss a new unconfigured transport constructor,
  an omitted/aliased wait timeout, or an unsafe overall-deadline reduction.
  The proportional follow-up also counts generic connect/wait member access and
  exact SDK client-symbol references so ordinary type aliases and extracted
  receiver aliases fail closed; intentionally obfuscated computed properties
  remain outside a source-shape contract and are covered behaviorally by the
  exact staged-artifact gate.
  Phase 6 does not close until mutation tests make those sibling regressions
  fail closed and the exact staged-artifact image gate passes.
- The accepted deadline fix uses 35,000 ms transport requests, at most 30,000 ms
  server long polls, named 110,000 ms Rust/Python internal bounds, and an honest
  120,000 ms TypeScript whole-process gate rather than a non-cancelling
  `Promise.race`. The final source contract passes 29 tests and rejects the
  original three-language defect plus unconfigured constructors, omitted/
  aliased/computed waits, lowered internal/external bounds, client aliases, and
  extracted wait receivers. Independent re-review reported no high/medium
  findings; the full container static gate also passes.
- The first clean `421f321` production candidate was
  `sha256:fa89405ca365dcf47b6ed1b80090f840fabcac1902cf7ced4dfc377e5af318f9`
  and its exact desktop fixture was
  `sha256:cf5ab62c433f726783906ffd95c3e25fd9f0f77e6c10cf3c30dd57821403feac`.
  The production lifecycle gate exposed a second real defect before either
  identity could be accepted: the daemon's five-second transport-only viewer
  monitor upgraded through websockify and read the RFB banner, then disconnected
  before sending the client version/security exchange. TigerVNC counted each as
  a security failure and, after five, returned `RFB 003.003` with the safe reason
  `Too many security failures`, intermittently degrading viewer readiness and
  failing the viewer-denial gate. A fresh diagnostic container reproduced the
  exact bytes and logs. Those image IDs are therefore rejected and must be
  rebuilt after the fail-first full-negotiation monitor fix. The regression
  models TigerVNC's threshold: under the old transport-only path the first five
  probes passed while accumulating incomplete security handshakes and the sixth
  received the blacklist banner. The recurring monitor now uses the sole
  bounded full RFB 3.8 probe through None-security selection, `ClientInit`, and
  bounded `ServerInit`; six consecutive handshakes leave zero incomplete
  attempts. Malformed and blacklist banners remain fail-closed, and the
  immediately following input probe preserves the X0tigervnc/XKB ordering.
  Focused tests passed 9/9, strict xenoteerd Clippy and Rustfmt passed, and an
  independent review found no high- or medium-severity issue.
- The clean `89d35fc` Phase-6 candidates were production
  `sha256:9993456020cdef89c43c38db1fca2a71268135c7983b5a260647995b50b0b2e3`
  and fixture
  `sha256:04a5828e018a2c79f5e721cd48dda878dd873c9c17b822cff73e6abdb81991a6`.
  Production lifecycle, Phase-4 event flood, real noVNC browser/RFB, desktop
  application matrix, Phase-4 live API, and Phase-5 AT-SPI gates all passed
  serially against those exact identities. The staged public-package gate then
  failed before accepting any artifact digest: the Rust example's first exact
  xmessage window wait returned a structured API problem after about 30
  seconds. These image identities are rejected and remain diagnostics only.
- That staged failure exposed two systemic contracts. First, the raw X11 actor
  evaluates `ManagedProcess` predicates before the daemon's asynchronous
  processd enrichment, so live query, resolve, and wait cannot match a public
  predicate the protocol supports. Broker-authenticated correlation must be
  committed to the single-owner revisioned model using exact window birth,
  reported PID, desktop generation, and expected revision fences; correlation
  changes must re-evaluate registered waiters. Second, window and accessibility
  waits legally accept 300,000 ms and 120,000 ms, while the generic outer
  handler timeout is 30 seconds. Equal 30-second example waits race into either
  typed `timed_out` or HTTP 504 `request_outcome_unknown`. Long-poll routes need
  explicit bounded headroom over their semantic maximum, and Rust/Python SDK
  calls need per-operation request deadlines as TypeScript already has.
- The actor-owned correlation closure must treat process lifecycle and model
  sequencing as part of the same authority boundary. Process exit, broker
  replay gaps/resync, stream failure, and reconnect invalidate committed high
  evidence before their effects are published. Every raw/accessibility/process
  correlation model mutation publishes a sticky change sequence before its
  response, and a correlation singleflight may cache success only under the
  exact actor-returned post-commit sequence; reading a newer sequence after the
  fact can relabel stale work and strand a registered waiter.
- A successful process-correlation RPC is not sufficient lifecycle authority
  while the broker event subscription is offline. Correlation starts
  unavailable, is enabled only after the atomic replay/live handoff is
  processed, and is disabled before subscribe failure, gap/resync, closure,
  stream error, or reconnect. The authority epoch fences in-flight RPC results
  at the model owner so high evidence cannot be recommitted during an event
  outage.
- The Phase-6 example audit found that semantic text previously proved only
  character counts. Closure adds a content-private AT-SPI backend comparison
  that returns only exact-match boolean evidence for unprotected fields, keeps
  protected fields length-only, and never exposes either requested or observed
  text in protocol data, diagnostics, or `Debug`. Exact readback now uses raw
  D-Bus `s` replies, rejects oversized/malformed bodies before typed decoding,
  and borrows the verified string without a second content allocation. The
  shared pinned zbus transport still necessarily admits its hardcoded 128 MiB
  raw-message allocation because it exposes no per-call receive-size knob.
- Rust `Desktop::with_control` may promise awaited release only after normal
  callback completion. Dropping/cancelling/panicking the outer future cannot
  await asynchronous cleanup. A renewal failure must fence new submissions and
  reach bounded cleanup even if the callback stops cooperating; it must not
  silently await that callback forever after renewals stop. The callback remains
  concurrently polled while a renewal exchange is pending, renewal-failure grace
  is capped at 250 ms, and abort evidence retains exact ambiguous in-flight
  command IDs. An ambiguous scoped release exposes its exact lease capability
  only through the explicit `lease_id()` recovery accessor; `Debug` and
  `Display` redact it.
- The executable Phase-6 gate is
  `scripts/sdk/test-public-quickstarts.py`. It reproduces the Docker build
  wrapper's source-tree identity, requires it to equal the image label, resolves
  and runs only one immutable image ID, safely extracts both Rust archives,
  installs the npm tarball plus Python wheel and sdist into isolated roots, and
  runs a typed invalid-auth probe followed by the canonical ten public behaviors
  for every variant in a fresh fixture container. Its unit, archive-install,
  identity/ancestry, source-fence, output-contract, and cleanup proofs pass. No
  Docker image was built during final closure, so the exact-image execution and
  identity record intentionally remain pending until the coherent Phase-6 image
  is built.
- Fixture ancestry labels and a production-layer prefix prove where derivation
  began, but not that an added layer did not shadow production paths. The public
  behavior gate inspects stopped base/fixture containers without executing
  either image, checks stopped state and image identity before and after copying,
  requires the inherited first-party manifest to be byte-identical, validates
  every manifest-listed hash in both root filesystems, and compares exact
  critical path/type/mode/symlink/content inventories. It also binds fixture
  source modes and bytes plus the artifact lock to the current repository, and
  requires all inherited Docker runtime configuration and labels to remain
  unchanged except the exact six validated fixture-only labels.
- Phase-7 transport hardening must operate before Axum/Tower request middleware:
  the accepted-connection permit, header count/bytes/read deadline, keep-alive
  idle bound, incomplete-request bound, and reserved health/shutdown capacity
  must be proven with raw sockets. Router-level body/rate tests are not evidence
  for this boundary.
- Phase-7 reserved operational capacity cannot be made reliable on the public
  listener because the request path is unknown until after headers consume a
  parser slot. Use the existing private `metrics_listen` seam for a separate
  loopback/private health and metrics listener, while preserving public health
  compatibility without claiming it is the reserved path.
- Phase-7 runtime authentication must add atomic complete token-set reload,
  metadata/expiry/revocation and scoped principals, including defined closure or
  reauthorization of already-active WebSockets. The existing multi-record auth
  library alone is not runtime rotation evidence.
- Runtime reload cannot reuse the current unlinked one-shot token handoff. The
  final design needs a GUI-inaccessible daemon-readable replacement or a narrow
  privileged reload broker, explicit SIGHUP handling, token-revision watches,
  and near-effect authorization revalidation so closing WebSockets cannot leave
  queued effects authorized.
- Phase-7 operations work must either add the currently missing persistent
  profile marker/migration/reset and drain seams or narrow the documented
  contract to ephemeral rematerialization plus bounded signal-driven shutdown.
  The aggregate OCI image license label should remain `NOASSERTION` with a
  separate first-party BUSL declaration and linked manifests; claiming the
  whole Debian image is BUSL would be inaccurate.
- Phase-7 environmental release observations that cannot be honestly reproduced
  locally include protected GitHub environments/tags, OIDC registry signing,
  public-registry clean-host verification, a genuine 24-hour active soak, and
  the supported-host LSM matrix. Implement their harnesses/workflows locally but
  record those external observations as unverified until executed there.
- The read-only Phase-7 AGIS territory/test preflight fixes the implementation
  order as transport/private operations, live authentication/revocation,
  telemetry/redaction, fault/fuzz/performance/leak evidence, then deterministic
  release bundles/workflows/operations documentation. Transport, authentication,
  and telemetry each touch security-critical server composition and must remain
  separate reviewed commit waves rather than one integration-sized change.
- Phase-7 transport composition must bind both public and private listeners
  before readiness. A connection permit covers the complete accepted
  connection; header count, parser bytes, header-read time, keep-alive idle, and
  drain behavior are distinct bounds. Raw-socket tests must prove public
  incomplete-header saturation cannot consume private readiness/metrics or
  prevent bounded SIGTERM.
- Phase-7 reload authority belongs in a separate narrow root
  `xenoteer-authd`, not processd. Its root-owned Unix socket authenticates the
  exact daemon UID/GID with `SO_PEERCRED`; bounded token-set sources are opened
  no-follow with owner/mode/type/inode checks and unlinked after handoff. The
  daemon retains only keyed fingerprints plus public metadata and atomically
  swaps a completely validated set. Version-one scope is the current desktop
  plus registered application IDs. Revocation/expiry/removal/scope reduction
  invalidates the principal incarnation, closes affected WebSockets with 1008,
  revokes owned leases/queued work, and rechecks authority at the first
  mutating effect.
- Phase-7 telemetry uses the existing request ID and fixed route templates,
  never raw URI paths or caller strings as metric labels. A comprehensive
  planted-canary gate must scan logs, problems, status, metadata, Debug/repr,
  audit records, and metrics. Fuzzing uses checked-in deterministic replay
  corpora for blocking gates; time-boxed libFuzzer exploration is nightly.
  Performance evidence retains distributions and names the reference hardware;
  local functional harness runs are not portable performance claims.
- The aggregate OCI license is already correctly `NOASSERTION` with a separate
  first-party BUSL declaration. The actual container metadata gaps are the
  stale `phase-2` profile label and omission of the existing `xenoteerctl`
  binary from the image/inventory. Release work must produce deterministic
  source/notices/SBOM/checksum/offline-verification bundles bound to source
  revision and exact image digest without claiming bit reproducibility until it
  is measured.
- The Phase-6 long-poll deadline patch passed independent source review with no
  high- or medium-severity findings: authentication precedes semantic body
  collection, matched-route classification ignores query/path values, typed
  pre-effect 504 problems are truthful, inner/outer headroom is separated,
  cancelled real waits release quota/actor state, and Rust/Python public wait
  operations carry per-operation transport deadlines.
- The final managed-process correlation design is actor-owned and revision
  fenced. Commits require the exact desktop generation, window birth, reported
  PID, model revision, lifecycle-authority epoch, and actor-returned post-commit
  change sequence. Exact-birth/same-PID evidence survives ordinary metadata
  refresh, but PID/birth changes, process exit, replay gaps, resync, malformed
  replay, stream error/closure, cancellation, or reconnect disable authority
  before externally visible effects. Reconnect uses cancellation-aware bounded
  exponential backoff, and only a successfully applied live event resets it.
- Correlation refresh uses one singleflight without holding its mutex across an
  await. Cached hits are revalidated at the actor boundary; an in-flight result
  cannot outlive a model or authority-epoch change. All ordinary list,
  snapshot, query, resolve, and accessibility projections scrub stale high
  evidence at their final authority gate, while managed selectors fail closed.
  The blocking final-snapshot path used by real window control applies the same
  exact epoch fence.
- Semantic waits register before correlation refresh, retain one immutable
  monotonic deadline through every refresh and recheck, and use one raw-event
  budget across reconciliation and all window snapshots. At exact deadline
  equality, a satisfied predicate wins; an unsatisfied predicate returns the
  typed timeout. Strictly post-deadline work cannot create a late match, and a
  transient unstable result re-registers only within the original deadline.
- Final Phase-6 source verification is green. Rust passed the complete
  workspace all-target/all-feature suite (including 326 xenoteerd unit tests
  plus 3 SIGTERM process tests), strict Clippy, Rustdoc/doc tests, schema and API
  checks, package-boundary tests, and both workspace/fixture dependency policy
  gates. Rust `cargo audit --deny warnings` scanned 274 locked dependencies
  against 1,173 advisories with no vulnerability. TypeScript passed 53 tests,
  all 73 conformance cases, and deterministic package inspection. Python passed
  49 isolated unit tests, all 73 conformance cases, Ruff, mypy across 19 files,
  and wheel/sdist inspection. The static container/release contract passed
  after removing generated local tool caches.
- The holistic pre-commit review caught one Python deadline defect before it
  reached an image: httpx scalar timeouts bound each connect/read/write/pool
  phase, not the complete response stream, so a drip-fed response could exceed
  the advertised operation deadline. The fail-first slow-stream regression
  reproduced that no timeout was raised. `request_with_deadline` now retains one
  outer `asyncio.timeout` even for deadline-capable transports, and
  `HttpTransport` independently bounds the complete exchange while preserving
  its per-phase httpx limits. Elapsed/internal timeout maps to
  `request_timeout`; caller cancellation remains `CancelledError`. Six focused
  regressions and the complete package matrix pass after the fix.
- Real native qualification is also green under the two-job low-priority
  policy: the authenticated isolated-Xvfb harness passed 11 X11 integration
  tests, 4 live capture tests, 2 adversarial daemon observation tests, and the
  independent XTEST fixture proof; the isolated D-Bus/AT-SPI harness passed its
  live registry and fixture probe. Independent final correlation and deadline
  reviews reported no high- or medium-severity findings. Exact coherent image
  and staged-artifact qualification remain deliberately pending.
- Clean source `0c7f9ff55a6f790bc167a3dc8bed18b52d9d7e3b` produced diagnostic
  production image
  `sha256:6c97c75e222bc602c71b9e9ddefbb91ada640580008f3a14703edc83e81b4588`
  and exact derived fixture
  `sha256:d1dddcb29771ad77dc990f7bc9e9827de6e8212ff8f9a445ef3e93dab1f248f9`.
  Production lifecycle/viewer denial, Phase-4 event flood, real noVNC,
  desktop-app matrix, a controlled Phase-4 live rerun, and the Phase-5 AT-SPI
  lane passed. The first Phase-4 live run truthfully returned
  `converged:false` for Chromium iconification, so these identities are
  rejected as release candidates despite the passing rerun.
- The iconification failure exposed a cross-connection handoff race. The
  window-control X11 connection can observe the ICCCM effect as converged, then
  `WindowControlRuntime` immediately obtains a pre-effect snapshot from the
  separate observation/model actor because queued X events are not a
  cross-connection reconciliation barrier. Public state/minimize translation
  derives its postcondition from that actor-owned snapshot and can therefore
  report a false nonconvergence. Closure requires a fail-first delayed-model
  regression and bounded exact-reference model reconciliation after the raw
  effect, without replaying the mutation; a genuine manager refusal must still
  return nonconverged.
- The handoff race is closed by an additive raw-observation snapshot barrier
  and exact actor-owned refresh. After a state/minimize effect, control waits on
  one absolute deadline for the observation connection to finish the snapshot
  round trip, drain its bounded ordered X11 event lane, update the model, and
  revalidate the original `WindowRef`; it never redispatches the effect. A
  stable target read failure tombstones that exact birth before bounded
  preserve-continuity reconciliation, and an ordinary same-XID replacement
  fences the predecessor and mints a new birth. An unsettled timeout, event
  overflow/budget exhaustion, or bounded-work coalescing instead invalidates
  every previously authoritative birth and defers recovery to a fresh-budget
  resync. Event-driven refresh uses an iterative bounded work queue so a
  Refresh immediately followed by Destroy/reuse cannot publish replacement
  bytes or wake a waiter under the old birth. Identical snapshots are semantic
  no-ops.
- The race fix is proven by delayed-model state/minimize regressions,
  genuine-nonconvergence/no-replay cases, target-loss and same-XID fencing,
  stable and unstable snapshot failures, deadline recovery, cross-window
  invalidation, 513-item work coalescing, nested Refresh/Destroy waiter
  rejection, and direct actor fault/cleanup coverage. Final source gates passed:
  23/23 X11 observation actor tests; 225 X11 library tests with 8 authenticated
  live cases ignored in the host-free lane; 338 daemon tests with 2 live cases
  ignored; full workspace/all-targets/all-features; strict workspace Clippy;
  doc tests and warnings-as-errors Rustdoc; and the authenticated native harness
  with 11 X11 integration, 4 capture, and 2 adversarial daemon-observation
  cases plus the independent XTEST fixture. Two independent final reviews found
  no remaining high- or medium-severity correctness, security, or API issue
  after the reviewer-requested fault-branch tests were added.
- Phase 3 is committed locally as `90b0781`; the verified Phase 4 boundary is
  committed locally as `83b044c`. Phase 5 has completed coherent exact-image
  qualification and closure review and is ready for its local boundary commit.

## Phase 4 implemented boundaries

- Window identity/query logic lives in `xenoteer-core`; raw X11 observation
  contains no desktop identity or authorization; authenticated projection is a
  server concern; `xenoteerd` is the sole composition/integration layer.
- The server now has an unavailable-by-default artifact service seam and a
  tested authenticated artifact upload/read/delete transport. The private
  immutable filesystem implementation lives in `xenoteer-artifacts`.
- The server now has an unavailable-by-default viewer-ticket service plus a
  bounded HMAC-digest-only, 32-byte CSPRNG, single-use registry and strict
  authenticated issuance route. The daemon composes it with the authenticated
  view-only gateway; viewer readiness is advertised only while the fixed
  loopback websockify/RFB chain passes its bounded probe.
- Viewer ticket wire timestamps use wall time, but security expiry is measured
  from a private monotonic origin; wall-clock rollback cannot extend a ticket
  and a forward clock correction cannot expire it early.
- Artifact GET supports exactly one bounded byte range and returns hardened
  content/range/digest/disposition/no-store headers; duplicate, multi-range,
  malformed, and unsatisfiable requests return an empty 416 after authorization.
- Pagination/reference tokens bind principal, desktop generation,
  revision/query/order, and expiry; observation waits use atomic
  check-register-recheck; old WindowRefs never retarget after XID reuse even
  after bounded tombstone/history expiry.
- A loss-triggered observation resync is an identity discontinuity: all prior
  live births must be invalidated and every still-observed XID must receive a
  fresh birth/reference unless event-sequence continuity is positively proven.
  Preserving by XID or matching metadata would let an old serialized reference
  retarget after a hidden destroy/recreate cycle. QueryTree inventory order is
  fallback discovery order, not EWMH stacking truth, and therefore must not
  populate `stacking_index`.
- Raw observed X11 atoms retain both the numeric atom identifier and an optional
  reviewed `KnownAtom`; unknown atoms remain diagnostic hexadecimal values.
  Root snapshots separately carry active-window, focused-window, and current-
  workspace evidence so daemon normalization does not invent those states.
- X11 focus evidence now distinguishes the raw focused child from a bounded
  (64-parent) QueryTree ancestry proof that focus belongs to one observed top-
  level target. Activation convergence requires both `_NET_ACTIVE_WINDOW` and
  that descendant-focus proof; raw XID equality is insufficient for real apps.
- Request JSON/schema objects are recursively closed, including strict wrappers
  for authority-bearing shared DTOs. Response JSON/schema objects are
  recursively additive. Window discovery responses carry
  `WindowSnapshotEntry { snapshot, reference_token }` so token-only resource
  routes are actually reachable without client token construction.
- Phase-4 public reachability is integrated for window control (including
  move-to-workspace), clipboard read/write/paste, screenshot capture/artifact
  persistence, process correlation, normalized events, and the view-only
  gateway. Compound atomic input, complete geometry policies, live
  capabilities, and the fixture matrix were included in the verified
  `83b044c` boundary.
- Managed window/process correlation must be broker-authenticated and bounded,
  not a trust upgrade applied directly to client-controlled `_NET_WM_PID`.
  Processd can batch-read `/proc/<reported-pid>/stat`, match either the exact
  manager leader PID/start-time or its verified process group against at most
  the configured live-process ceiling, and return only exact `ProcessRef`
  claims plus typed evidence. The daemon then applies fenced asynchronous model
  enrichment; exact leader evidence maps to `NetWmPid + ProcStartTime`, a
  descendant maps to `NetWmPid + ProcessGroup`, and a uniquely correlated
  client-leader window can add `ClientLeader`. Missing evidence stays low/none,
  and disagreement with stronger client-leader/group evidence sets `conflict`
  rather than silently selecting the reported PID.
- The public viewer transports a single-use ticket in a dedicated WebSocket
  subprotocol while selecting only `binary`; the URL path/query, logs, close
  reasons, and backend handshake never receive ticket bytes. Admission order is
  route/origin/protocol and capacity, atomic ticket consume, then the fixed
  loopback websockify connection. Invalid, replayed, or wrong-origin tickets
  therefore cause zero backend accepts; a backend failure after consume is a
  fail-closed spent ticket and callers must mint another.
- Clipboard actor event backpressure latches a content-free resync barrier.
  Once the bounded queue fills, later incremental events are discarded until
  the consumer drains the pre-loss prefix and observes that barrier; daemon
  normalization then requests a global model/event resynchronization.
- Capture prefers MIT-SHM 1.2 using one exact-size server-created segment per
  in-flight owner-thread request. It waits for the image reply, reads only the
  reported bounded body, checks detach, and never reuses a segment. Any SHM
  protocol/segment failure disables SHM for that actor, falls back to core
  `GetImage`, and records successful fallback in content-free health counters;
  no unsafe Rust or
  persistent browser-competing shared-memory pool is introduced.
- Ubuntu Xorg/Xvfb 21.1.12 emits exactly one structural XKB SetMap
  `KEYCODES|GEOMETRY` notification on the first XTEST keyboard request, even on
  bare Xvfb. The input actor exempts only that first, single generation+1 event
  when its negotiated opcode/device/range match, a complete serialized-keymap
  fingerprint is unchanged, every used binding is physically equivalent, and
  a final zero-invalidation bracket passes. All ordinary/repeated/semantic or
  post-first-effect mapping changes remain fail-closed.
- Browser clipboard paste can perform a compatible probe and the conversion
  consumed by the renderer on separate event-loop turns. Temporary ownership
  now remains for a 250 ms quiet interval after the latest request or transfer;
  restoring after the old first-transfer-only 50 ms interval caused Chromium
  to paste the preserved clipboard value. The watcher has deterministic clock
  coverage and the live browser postcondition proves the fix.
- A strict window-relative multi-click is not one long-lived authorization:
  after each host-side dwell/interval, the input owner thread drains X11,
  resolves the live client/frame geometry and pointer again, and rechecks the
  exact observed birth plus focus immediately before each zero-delay
  `ButtonPress`. Focus loss, XID reuse, or geometry drift before press two
  suppresses that press and returns partial-effect evidence for press one.
- Command cancellation uses one shared mutation-grant vocabulary at REST,
  WebSocket, and daemon boundaries. Window and selection mutations are ordinary
  cancellable completions rather than identity-preserving atomic completions;
  a stop observed before actor admission has `BeforeEffect` evidence, while a
  stop after the final X11/clipboard boundary waits for bounded evidence and
  reports `AfterEffect` conservatively without re-executing an exact retry.
- The Phase 4 live fixture uses GTK's multiline `GtkTextView` for its 384-KiB
  INCR case because `GtkEntryBuffer` silently truncates at 65,534 characters.
  Clipboard restoration evidence is honestly `partial_value_copy`: exact
  bounded text is independently verified, but the prior owner identity and
  arbitrary non-text targets cannot be reconstructed.
- Capped live matrices pass exact GTK3, Qt6, Chromium, and Firefox direct/INCR
  paste, byte-exact value-copy restoration, root plus five window captures,
  move/resize, and minimize. QtWebEngine remains mandatory for window, AT-SPI,
  browser, sandbox, initial-value, and capture coverage, but its exact clipboard
  insertion is isolated: QtWebEngine 6.8.2 with PyQt 6.9 on X11 duplicates one
  four-event paste
  chord whenever forced accessibility or AT-SPI focus activates its
  accessibility path. Direct libXtst and DevTools probes ruled out daemon
  replay, X autorepeat, duplicate transfer, and AT-SPI readback. Its HTML input
  also exposes Text but not EditableText, so a semantic fallback would be a
  false capability claim. The upstream/toolkit-adapter follow-up remains open.
- All external desktop-event ingress now treats a lost X11 or process-event
  batch as one shared epoch boundary, not merely a model rebuild request. Odd
  atomic epochs coalesce loss; producers admitted on an even epoch stamp bounded
  queue entries; the sole relay claims the next even epoch before publishing
  `history_lost` and drops any entry whose admission epoch differs. This closes
  load-before-gap/enqueue-after-claim, select-before-gap/publish-after-barrier,
  and cross-relay process-gap races without blocking X11 producers, and the live
  flood gate proves a pre-gap reference is rejected while a still-live reused
  XID is reminted afterward.
- WebSocket unsubscription acknowledgement is ordered with respect to the
  control stream, not ahead of already queued events. The event-flood fixture
  therefore drains only valid events for the replacement subscription while it
  waits for the correlated acknowledgement, with strict message and time
  bounds; unrelated events and resynchronization still fail the proof.
- The viewer acceptance path traverses ticket issuance, the authenticated
  public gateway, websockify, X0tigervnc, and an independent RFB observer. It
  proves keyboard, pointer, clipboard, and resize messages cannot affect the
  desktop, proves one-use replay rejection, and bounds aggregate fragmented
  WebSocket/RFB buffering as well as individual frames.
- The first coherent-image run caught a stale pinned noVNC module closure:
  Debian noVNC 1.6 places DES under `core/crypto/` and `core/rfb.js` now imports
  additional crypto and decoder modules. The gateway allowlist now matches the
  recursively resolved ES-module closure rooted at `core/rfb.js`; startup still
  rejects any missing, symlinked, empty, oversized, or over-budget module.
- A derived noVNC spike must download pinned archives even when the coherent
  base image already has one of those packages installed. Its checksum stage
  uses `apt-get --download-only --reinstall`; without `--reinstall`, APT can
  omit an installed package's archive and make the supply-chain proof depend on
  base-image cache state. Spike build/runtime CPU and memory are capped at two
  CPUs and 6 GiB, and the resulting real-browser/RFB gate remains view-only.
- Hardened-container repetition exposed a normal X11 lifetime race: an XFCE
  window can disappear between the root inventory, checked event subscription,
  and detailed snapshot. Reconciliation now retries a failed snapshot once,
  omits only a repeatedly vanished member, records `VanishedMember`, and commits
  the coherent surviving inventory. A transient `BadWindow` no longer poisons
  the observation service or shuts down otherwise healthy physical automation.
- Parallel process-level SIGTERM tests exposed a separate lifecycle race: an
  observation event selected before shutdown could fail after the shutdown
  request and be misclassified as fatal model poison. The model loop now stops
  admitting events once shutdown is visible and lets requested shutdown win an
  already-selected failure, while the same failure before shutdown remains
  fatal. A forced interleaving and 20 parallel process-suite repetitions cover
  both the classification and the real daemon teardown path.
- The concurrent host-X11 proof must launch Xvfb with `-noreset`. Otherwise the
  server resets when one test binary closes the final client, and the next
  binary can receive `ECONNRESET` during its opening handshake. Two parallel
  harnesses reproduced that gap at the same poll-flood test; keeping each
  authenticated, isolated Xvfb alive until explicit harness cleanup removes the
  lifecycle race without changing production observation behavior.

## Phase 5 accessibility baseline

- Semantic availability is independent of the required physical desktop.
  `DesktopSupervisor` owns X11/input (and the optional viewer) only; AT-SPI
  connection loss must degrade the accessibility capability, increment its own
  generation, and reconnect without failing global desktop readiness or
  withdrawing healthy physical input.
- Accessibility configuration is fail-closed and bounded at every admission
  surface: actor requests/events/waits, cache/tombstones, selector depth,
  traversal/match/snapshot nodes, encoded snapshot bytes, per-proxy/whole-query
  deadlines, and reconnect backoff. The initial cache/query/snapshot ceilings
  match the reviewed plan: 100,000 / 25,000 / 10,000 nodes and 16 MiB.
- `accessibility:read` and `accessibility:write` are separate grants. Only the
  write grant authorizes semantic effects and command cancellation. Protected
  text still requires operation-specific policy; possessing the read grant
  does not make secret field contents observable.
- Phase 5 request schemas expose only value metadata, content-free text
  metadata, and Component bounds. Accessible text content, action lists,
  accessible IDs, attributes, relations, and their selectors stay reserved
  until a bounded on-demand hydration lane exists. Component predicates and
  geometry waits use only explicitly labeled AT-SPI-screen coordinates and
  reject incomplete live bounds instead of treating absent evidence as a
  non-match; root-physical conversion requires an explicit desktop profile.
- The pinned `atspi-connection` 0.14.0 `remove_match_rule` helper accidentally
  adds a match rule, and `register_event` installs Registry forwarding before
  the D-Bus match. The adapter therefore owns five narrow raw signal-only zbus
  streams (Cache, D-Bus owner, Object, Focus, and Window), uses only
  reference-counted Registry forwarding helpers, and lets stream teardown
  perform the real D-Bus `RemoveMatch` operations. The streams are sequence-
  merged by zbus receive position before classification; comparing independently
  scheduled streams would create false non-monotonic gap reports.
- zbus signal queues backpressure the shared socket reader when full. A
  dedicated drain continuously consumes the ordered merge and uses only
  nonblocking bounded admission; overflow advances a capacity-independent
  resync epoch instead of awaiting the semantic cache actor. The configured raw
  signal capacity is partitioned across the five streams, and the 256 KiB
  maximum normalized item size keeps the default actor/backend/public queues at
  a byte-derived capacity of 512 within the 128 MiB runtime cache ceiling.
- Object metadata events are published only inside an equal generation/revision
  mirror fence. Covered older events and all events arriving while a rebuild is
  already pending are suppressed; the first forward coordinate fences and
  requests one rebuild, so an ordinary GTK state burst cannot amplify into a
  public resync/reconnect flood.
- Semantic mutations dispatch exactly once. Focus, value, selection, editable
  text, and scroll evidence use bounded deadline-aware read-only settling with
  exponential backoff and last-valid-sample behavior. Text length must converge
  before caret/selection policy can issue its follow-up mutation; no retry loop
  is allowed to repeat the original effect.
- Accessibility page cursors are actor-owned, principal/query/revision bound,
  and expire within 30 seconds. Application/object references instead carry
  explicit connection, owner-instance, and object-birth generations; a stale
  reference is never repaired by rerunning its selector.
- Window accessibility correlation is committed through the single-owner X11
  observation model using exact `WindowRef` births. It survives ordinary X11
  metadata refresh, clears on semantic gaps, emits ordinary window-change
  revisions, and rejects XID reuse before mutating query-visible state.
- The live large-tree fixture deliberately materializes 4,096 stable accessible
  rows so Cache traversal and pagination reach every row. This is not described
  as virtualization: the standard GTK3 and Qt6 fixtures exercise their native
  virtualized controls separately because GTK Cache `GetItems` exposes only the
  currently materialized subset of some virtual widgets.
- The dedicated depth-budget case builds a valid depth-24 topology and queries
  `Phase5 Deep Node 023` with `max_depth=8`, producing the public
  `query_budget_exceeded` result instead of conflating a query budget with an
  actor-level malformed-topology rejection.
- The qualification runner covers GTK3 and Qt6 process-restart fencing plus
  Chromium and Firefox document-reload fencing. An intentional accessibility-
  bus replacement advances only the AT-SPI generation. Some toolkit bridges
  retain a dead bus connection for their process lifetime, so the gate first
  proves that the old reference is rejected, then relaunches a controlled
  toolkit client and proves a fresh reference on the replacement bus without
  changing the desktop generation.
- Event pressure is a bounded 5,000-mutation producer with both a normal client
  and a deliberately slow subscriber. The proof requires a content-free resync
  barrier, rejects the pre-loss reference, relaunches the controlled producer
  when its toolkit bridge retained the old connection, and verifies fresh
  ingestion while the container remains inside its CPU, memory, PID, and shared
  memory limits.
- The live adversarial application supplies a 70,000-byte accessible name and
  must remain isolated while a healthy GTK sibling stays queryable. Bad-parent
  and cyclic-topology traversal are bounded pure-model/unit cases; the live
  fixture's self-relation is not claimed as proof that relation hydration is
  implemented in Phase 5.
- The Phase 5 source passed the full workspace all-features/all-targets tests,
  strict Clippy, Rustdoc, schema/API-documentation, container-static,
  dependency, audit, license, and native-X11 gates. Final coherent
  qualification used production image
  `sha256:68508e98bb1f7a0995e96b4b93499cced7247fa7a99f90652c19abec2a52dafb`
  and its exact derived desktop-app fixture
  `sha256:1733ddadd8d2235c42ec518bbc06d2053e6eded9d6f4cebd6999708f9470e934`.
  Production, desktop matrix, Phase 4 live API, and Phase 5 AT-SPI gates all
  passed with no daemon override and the two-CPU resource policy. The Phase 5
  runner covered GTK3/Qt6 restart fencing, Chromium/Firefox reload fencing,
  private-P2P denial, missed-event polling recovery, semantic/physical effects,
  stress, reconnect, and flood cases.
- Performance qualification is intentionally Phase 7 work. No Phase 5 result
  claims 10,000-node cold-snapshot timing, selector p95, event-lag, stable cache
  RSS, or a large-browser soak measurement.

## Phase 6 developer-experience invariants

- Phase 5 is committed locally as `8a49c3f6983f9761ebcbc088703ae0575d5f3a19`.
  The immutable qualification images above are the exact Phase 5 boundary.
- The frozen release-one protocol range is exactly v1.0. Public unbounded
  counters are canonical decimal strings, never JSON numbers; signed forms,
  whitespace, leading zeroes, overflow, and numeric JSON are rejected.
- The public Rust, TypeScript, and Python SDK packages plus the language-neutral
  conformance corpus are Apache-2.0 boundaries. They must not depend on or
  package any BSL server implementation. `xenoteerctl` remains BSL with the
  server repository and consumes only the public Rust SDK.
- SDK mutation helpers must expose a caller-retained command ID and exact,
  redacted canonical envelope before any network I/O. An ambiguous send or
  local cancellation must not consume that submission. SDKs never replay an
  effect automatically; lookup and any explicit same-ID/same-body resend remain
  caller-controlled.
- SDK event streams must establish a correlated filtered subscription before
  appearing live. They bound frames before allocation, preserve unknown
  additive messages, use heartbeat/read-staleness checks, distinguish permanent
  handshake/auth failures from reconnectable transport loss, and explicitly
  surface resync, local overflow, generation change, and terminal close reasons.
- Direct SDK transport accepts HTTPS/WSS with platform roots. Plain HTTP/WS is
  restricted to numeric loopback origins. Long-lived API tokens remain in the
  Authorization header and never enter URLs, subprotocols, Debug/repr, errors,
  or telemetry.
- JSON response limits are endpoint-specific: ordinary control JSON is small,
  valid accessibility results can reach 16 MiB, clipboard artifacts 16 MiB,
  and other private artifacts 32 MiB. Artifact transfer is bounded streaming
  with exact scope, length, purpose, content type, and SHA-256 validation.
- Source `aff69fadc10c506c2837099d6d866fe4348d425a` produced coherent
  diagnostic production image
  `sha256:0fea36dfbf24dab8a5c17cae7da3a519930d4e5dfa34fe94077ff5bf92d99799`
  and exact derived fixture
  `sha256:7272c87b60b8513ea7bde327529dba419f7f8469b5c736f2d51076df6895c488`.
  Production acceptance, Phase-4 live API/event-flood, real noVNC, and the
  normal+hardened desktop-app matrix passed. The first Phase-5 live run failed
  protected `element_set_text` as `backend_failure / none / never`, so both
  image identities are rejected and no controlled rerun can qualify them.
- That failure exposed a latent AT-SPI pre-dispatch classification defect.
  Backend ingress epoch changes and legitimate second-read drift in action
  metadata, text evidence, admitted semantic identity/state, fresh observation,
  and targeted refresh are now one typed `PreDispatchConflict`. Mutation,
  observation, and reconcile paths preserve that no effect was dispatched; the
  daemon may reacquire evidence once and then returns
  `stale_reference / none / after_resync` if conflict repeats. First-read
  invalidity, malformed/oversized/unsupported data, decoding and limit failures,
  and generic protocol errors remain terminal. Any backend failure after the
  dispatch marker remains outcome-unknown and permits only same-command-ID
  dedupe/retrieval, never a new effect replay.
- Fail-first tests reproduce the original generic-Protocol result at the permit
  boundary and all five live evidence-drift families. Composition tests prove
  one conflict yields two attempts and exactly one dispatch for protected
  set-text and semantic text-insert; repeated conflicts yield two attempts and
  zero dispatches; generic pre-dispatch Protocol yields one attempt/zero
  dispatches; post-dispatch Protocol yields one attempt/one dispatch. Protected
  text remains absent from all evidence and diagnostics. The expanded source
  passed all-target/all-feature workspace tests, strict Clippy, doctests,
  warnings-as-errors Rustdoc, the isolated live AT-SPI fixture, and the full
  authenticated X11/XTEST/capture/daemon native harness. Two independent final
  reviews found no remaining correctness, security, or test-realism issue.
- `BackendFailureKind` and `SemanticError` remain exhaustive internal Rust
  boundaries so future effect classifications force every consumer to update.
  Adding `PreDispatchConflict` is an intentional pre-1.0 source break in a
  `publish = false` server-side crate; no wire protocol or public SDK contract
  changed.
- Source `e63f52d260490f64eb1deb36317227e9f7eb99d5` produced diagnostic
  production image
  `sha256:61a92b0283d8c91b15af572a184ae7481801c4c44f9d9fb0d27fd46f29f5becf`
  and exact derived fixture
  `sha256:79ef2dfef38d3cbe11ad3f2a797abc8e84831daadd85947f4c3daf6699fe8aa7`.
  Their source-tree identity is
  `ba14350aca3b32c7183837d768df5f79f349877b1e4d82f39415ec2b85a056e2`;
  the fixture records the exact production ID and preserves its 28-layer
  prefix. Phase-5 AT-SPI, production lifecycle, Phase-4 live/event-flood,
  noVNC real-browser/RFB, and normal+hardened desktop-app lanes all passed on
  their first run. The first public quick-start run failed before accepting
  any package digest or live SDK result because nested
  `sudo -H -u #1000` reset `PATH` and hid the user's NVM npm/Node tools.
  Therefore both image identities are rejected release candidates despite
  their runtime passes.
- The public package runner now resolves every Cargo/npm/Node/Python executable
  through a canonical, target-identity-aware `PATH`, filters unavailable or
  untrusted host entries, preserves symlink proxy names needed by npm and
  rustup, and launches package builds through a clean `HOME`/`PATH`
  environment. Target-primary-group-writable tools are permitted because the
  build already executes as that exact UID/GID; foreign, supplemental-group,
  other-writable, inaccessible, or relative paths fail closed.
- Installed Rust, npm, wheel, and sdist quick-starts no longer execute under
  the root gate process. The subprocess boundary requires non-root UID and
  GID, clears supplementary groups, passes only the explicit quick-start
  environment, keeps bearer values out of argv and diagnostics, and uses the
  strict-resolved installed artifact root as CWD (and Python path). This
  prevents repository-source shadowing and preserves artifact-only evidence.
  The boundary passed 48 focused/contract tests, actual nested-sudo npm/Node
  discovery, and an actual root-to-UID/GID probe observing UID/GID 1000,
  no supplementary groups, and only the allowlisted environment. Independent
  reviews found no remaining package or privilege-boundary issue.
- Source `b01405a2633616c40caa88e59d0077016664dfde` produced production
  `sha256:a650e129c80c203097db93c43370713fb202d2391bf58ebbe0507576f9a7bfc7`
  and exact derived fixture
  `sha256:356225fe5a1548cfd82f127863a529b45c9b6ef86d72541b55d30b38d0dc6180`.
  Both record source-tree
  `05a7a1afc0e485c8fe5c9a774a8ba5eab5e29b3151e29a7e086e4abdb973304b`,
  dependency-lock
  `0aecfd5eecdedf7a250f2d69a54d6b60aee506c4bc8f149ac4c7788dd6fe81d4`,
  and `dirty=false`; the fixture records the exact production ID and preserves
  its 28-layer prefix in 32 total layers.
- The `b01405a` pair passed first-run Phase-5 AT-SPI (55 seconds), production
  lifecycle/security (7 minutes 41 seconds), and Phase-4 live fixtures
  (24 seconds). Lane 4 did not exercise the image: the prescribed root
  invocation gave `test-phase4-event-flood.sh` sudo's secure `PATH`, but the
  script required ambient `cargo`/`rustc` before its later `SUDO_UID` and
  invoking-home toolchain resolution. It exited 77 with
  `required command is unavailable: cargo`; lanes 5-7 were not run and lane 4
  was not rerun. The pair is rejected because qualification is incomplete.
  Failure log
  `/tmp/codex/xenoteer-b01405a-phase4-event-flood.log` has SHA-256
  `6abd13549ea6c903062a0b0d6a20db90d6e8f890670b2a5be372ffd326e044ce`.
- Before the `b01405a` matrix, 13 documented obsolete/rejected Xenoteer image
  tags with zero container references were removed explicitly (no prune,
  cache, volume, builder, or unrelated image removal), recovering
  10,876,547,072 bytes. The exact cleanup log is
  `/tmp/codex/xenoteer-b01405a-obsolete-image-cleanup.log` with SHA-256
  `f5d3d2345d3a03c79c630d462d8583e7550e27b21dee9be2426121e7ab300b58`.
- The event-flood host runner now resolves the invoking account before Rust
  tools, validates every selected home/tool path and ancestor for canonical
  ownership/mode/traversal safety, preserves rustup proxy names, and crosses
  root-to-user execution through trusted `sudo` plus `env -i`. Only HOME,
  CARGO_HOME, RUSTUP_HOME, the validated absolute RUSTC proxy, explicit PATH,
  and C.UTF-8 locale cross that boundary. Exactly one lowercase path-safe Linux
  target triple is accepted before it becomes a Cargo target or path component.
  The resolver works with invoking-user proxies and trusted custom absolute
  Cargo/Rustc locations without admitting their directories into the clean
  PATH.
- The fail-first regression reproduced the exact lane-4
  `required command is unavailable: cargo` result and separately proved that
  ambient Rust overrides crossed the first implementation. The final dedicated
  suite passes 21/21 as the normal user (including a real passwordless-sudo,
  secure-PATH UID-1000 run) and 19 passes with two explicit integration skips
  as root. It covers malformed identity/account/home/tool inputs, path and
  permission trust, rustup proxies, ambient overrides and secret canaries,
  custom absolute tools, ambiguous/malicious target output, direct/rootless
  invocation, and all sibling discovery contracts. `bash -n`, ShellCheck,
  Python AST parsing, the complete static container gate, diff checks, and
  two independent review rounds are green.
- Repair commit `47b5dbfe27c9b97887d97e9a0859a2c1e4c7b766` produced exact
  production image
  `sha256:e8b60118065d3d4d83418d3f77b5a62f1a6cee21a84968c20e4b02a1aca3520b`
  and exact derived fixture
  `sha256:a29e8d8b482c5ea32315ebb2b443960c595a3d6c618838deda0b3b71d5b44835`.
  Both record clean source-tree
  `f92b67c11e97339020c5f811d925d783f3f5fdcd210253a55d019ffd587e62a5`
  and dependency-lock
  `0aecfd5eecdedf7a250f2d69a54d6b60aee506c4bc8f149ac4c7788dd6fe81d4`;
  the fixture records the exact production ID and preserves its 28-layer
  prefix in 32 total layers.
- The first and only Phase-5 AT-SPI live run against that `47b5dbf` pair failed
  after 38.37 seconds at the cursor-bound accessibility query. Lanes 2-7 were
  not started and lane 1 was not rerun, so both image identities are rejected.
  The visible daemon evidence shows repeated structural/targeted AT-SPI rebuilds
  and generation changes before the query failure. The exact failure log is
  `/tmp/codex/xenoteer-47b5dbf-phase5-atspi-live.log` with SHA-256
  `bcf74161af1128f649f7d108dd3de71927cf86b0f09455db782060eb43b921eb`.
  Diagnosis must reproduce and close the cursor/revision failure class before
  another coherent image pair is built; rerunning the rejected candidate is not
  acceptance evidence.
- The cursor failure now has a preserved fail-first reproduction at
  `/tmp/codex/xenoteer-47b5dbf-phase5-pagination-red.log` (SHA-256
  `ed26f2b1ea85eafa2a32e2136c1d5d366030828d4bd5e34bd19dbeee4dc8602d`):
  the former live collector consumed page one, received the daemon's intentional
  409 stale cursor after an AT-SPI cache mutation, and aborted instead of
  restarting the complete pagination transaction. The repair retries only the
  exact 409 stale/toolkit-resync and 429/503 backoff contracts within one
  60-second/16-transaction/80-request/40-page budget. Independent review still
  gates acceptance and must verify that every successful page is bound to one
  desktop/AT-SPI generation/snapshot revision and that all 96 expected names
  are checked exactly rather than only by count/endpoints.
- The independent pagination review confirmed those two gaps as release
  blockers. Its fix now freezes desktop ID/generation, AT-SPI generation,
  snapshot revision, and order for every successful transaction; validates
  strict application/element scope, identity, revision, warning, traversal,
  cursor, and `Retry-After` contracts; resets metadata only on a whole
  transaction restart; and compares the exact ordered 96-row stress result.
  The focused hardening suite has 21 tests, and complete container-script
  discovery passes 42/42. Primary RED evidence is
  `/tmp/codex/xenoteer-phase6-pagination-hardening-red.log` (SHA-256
  `edb6f421cfb94e953bbe21b1e598b8552f7d2bafa9d2582af9ebb7b4c7a57bbb`);
  final GREEN evidence is
  `/tmp/codex/xenoteer-phase6-pagination-complete-green.log` (SHA-256
  `713c763b2d0db53f3693982556151bd2c11bca056d1341dd3061660e72c55ec6`).
  A new immutable-image Phase-5 lane remains mandatory because the original
  rejected image log did not retain the HTTP problem body/status.
- The Phase-6 packaged-doc/runtime audit corrected the Python README's obsolete
  selector forms, split SDK CI into one Rust job plus Node 22/24 and Python
  3.11--3.14 matrices, and moved the unqualified published Rust Phase-3 example
  into the internal non-publishable `fixtures/phase3-sdk-smoke` crate. The
  authoritative Rust archive verifier must be rerun after the concurrent Rust
  options work refreshes both Cargo locks.
- The connection-options audit found a security defect in the TypeScript safe
  logger: raw request paths can contain desktop, command, lease, and artifact
  identifiers despite the public no-ID promise, and streaming downloads plus
  WebSocket attempts bypass logging. Python has no safe logger. Both languages
  also split HTTP/WebSocket adapter policy and ownership, can leak failed
  handshake candidates, and do not document that same-origin WSS derivation
  cannot copy CA roots, mTLS identity, proxies, pinning, DNS, or agents from the
  HTTP adapter. Phase 6 now requires closed route-template log events, exact
  attempt lifecycle evidence, explicit borrowed/owned adapter semantics,
  failed-socket cleanup, retained reconnect policy, and paired TLS/proxy
  guidance before another image build.
- The first independent connection-options review rejected the author-green
  Rust/TypeScript wave. TypeScript awaited rotating token providers outside the
  effective HTTP/WebSocket deadline, could leave the established failed socket
  open while reconnecting, and used an uncancellable reconnect sleep. Rust
  could lose the terminal half of a safe HTTP log pair when its attempt future
  was dropped by timeout/client/caller cancellation, and sent WebSocket hello/
  subscription frames outside the connect deadline. Each finding requires a
  fail-first cancellation/stalled-peer regression and independent re-review;
  author-green package/conformance results are not closure evidence yet.
- The completed Rust/TypeScript review also found that TypeScript retried
  normal/protocol-terminal WebSocket close codes, could yield artifact bytes
  before validating mandatory exact length/digest headers, and allowed
  case-insensitive caller header collisions with SDK-owned authority/framing.
  Rust retained a failed established WebSocket through reconnect backoff and
  replacement attempts. The repair wave must prove exact terminal/transient
  close classes, pre-yield integrity rejection, reserved-header rejection, and
  old-peer closure before replacement.
- Rust `catch_unwind` cannot make callback panic payloads secret: the process
  global panic hook runs before the unwind is caught and may emit the payload.
  The SDK may erase ordinary provider/hook errors and catch panics so they do
  not escape or change outcomes, but it must document panic-hook output as
  caller/runtime responsibility and forbid secrets in panic payloads. Per-call
  global hook swapping would introduce a worse concurrent-process race.
- Python's broadened connection-boundary regression reproduced all four tested
  Unicode control-character metadata canaries being accepted and confirmed
  that no public `connect_timeout` bounded token/factory/hello/welcome work.
  Primary API RED evidence is
  `/tmp/codex/xenoteer-python-connection-deadlines-api-red.log` (SHA-256
  `93a75db20415ba5e78e68609ca36813a907fde182a2d102d2ccd3915d39eb59f`);
  a separate full focused run hung exactly at the unbounded provider and is
  corroborating failure evidence, not a successful test.
- The independent CI/package review rejected the file named
  `requirements-test.lock` as an exact lock: it pins versions but not artifact
  hashes, CI omits pip `--require-hashes`, and the existing source contract
  rejects hash continuations. A same-version index artifact could therefore be
  replaced without changing the repository. Phase 6 now requires a hashed lock
  compatible with every Python 3.11--3.14 CI runtime plus negative tests for
  missing, altered, malformed, and unhashed entries.
- The broadened TypeScript/Rust lifecycle repair has genuine fail-first
  evidence: `/tmp/codex/xenoteer-ts-lifecycle-red.log` (SHA-256
  `298b304a54e26e3c73e01f2b883456bb4a6cadd90065b4088a1148a2ce79e50b`)
  records ten behavioral failures plus the unbounded-provider watchdog, and
  `/tmp/codex/xenoteer-rust-lifecycle-red.log` (SHA-256
  `dd8ec4f9056882c04bd9166cfedc914baa9be6822e17ff1043d9a1634db515ea`)
  records the missing bounded stalled-WebSocket-send behavior. The TypeScript
  focused repair is 11/11 green; Rust compilation and complete gates remain
  pending.
- The final Rust/TypeScript connection-policy author gates are green.
  TypeScript passes 80/80 tests, the immutable 73/73 conformance corpus, and
  its deterministic 61-file package check; the final test log SHA-256 is
  `b8fd1df5b8906d77ee51f8da00aff290e586ab50516329dee3b1069621be4bab`.
  Rust passes 69 unit, 4 conformance, 7 connection-option, 1 package-boundary,
  2 lifecycle, and 5 TLS tests plus strict Clippy, warning-denied docs, and
  exact package listing; its final test log SHA-256 is
  `55964aeb39b49a9dd411c4dec25df9bb570a46dae7b6021cd8ecf13507904b95`.
  Adjacent fail-first evidence caught provider side effects before
  reserved-header rejection and terminal close 1000 consuming a retry
  (`/tmp/codex/xenoteer-ts-adjacent-red.log`, SHA-256
  `bd71e91cbea60ebf17d3aaed8982fa111df5cb5d94c1260d92bde251f735e3fb`);
  its focused GREEN SHA-256 is
  `059cccacd00a2073f9cd7b4b5a4bba48a797953c150f2f698a22ff254f66c58c`.
  Independent repair re-review remains mandatory before closure.
- The independent repair re-review found one remaining medium Rust defect:
  HTTP/artifact token resolution used its own relative timeout without selecting
  client cancellation, then the actual HTTP operation received a fresh full
  timeout. A hung provider could survive `Client::close()` for up to the
  configured 300 seconds and provider latency plus I/O could consume nearly
  twice the documented end-to-end deadline. The common request path must use
  one absolute deadline across provider and transport work.
- The same re-review found two adjacent Rust WebSocket gaps. Policy close codes
  before welcome (`4401`, `4403`, `1008`) were collapsed into generic protocol
  failure rather than exact terminal authentication/permission errors, and
  established heartbeat/pong writes still used an unbounded send outside client
  cancellation. Both need fail-first initial/reconnect/blocked-sink coverage.
- Rust also marked an HTTP safe-log exchange successful as soon as response
  headers arrived. Body stall/truncation/oversize/malformed decode or
  cancellation after headers could therefore return an SDK failure with a sole
  `Succeeded` terminal event. The guard must span bounded body collection and
  decode under the same absolute deadline and publish exactly one truthful
  terminal.
- The four-finding Rust follow-up has preserved behavioral RED
  `/tmp/codex/xenoteer-rust-four-findings-behavioral-red.log` (SHA-256
  `810addc6a24fe5665601f50287e9af83c15ac592287ada62f61fb8597e83c098`)
  and missing established-send RED
  `/tmp/codex/xenoteer-rust-established-send-red.log` (SHA-256
  `31a5eb2d5d699b612bd8a36645c0d77c23094e015f6fd9fb90ed13bd4f9fb9f0`).
  The author-green implementation uses one monotonic HTTP deadline through
  provider/request/body/artifact/decode, spans safe-log state through final
  semantics, maps pre-welcome policy closes exactly, and bounds/cancels
  established heartbeat/Pong writes. Final SDK evidence (75 unit plus every
  conformance/connection/package/lifecycle/TLS integration) has SHA-256
  `a7500a81651dfb37cbbbc836efbecb5350c792ec2a1a72598d287db4d01902ab`;
  Clippy, rustdoc, formatting, and package listing are green. Independent
  post-repair verification remains pending.
- Python now rejects synchronous token callbacks before I/O and accepts only a
  static token or cancellation-cooperative async provider. Five repeated
  cooperative provider timeouts prove five cancellations/finalizers, zero
  active or pending provider tasks, and zero adapter I/O. This does not claim
  Python can preempt hostile callback code that blocks the event loop or
  suppresses cancellation. CI hardening RED evidence is
  `/tmp/codex/xenoteer-python-ci-hardening-red.log` (SHA-256
  `7c55aeeedf5fef7a6f3a853c94dab7036eb18345edc3f0ac37b429708ca7f27b`);
  the 22/22 GREEN contract log is
  `/tmp/codex/xenoteer-python-ci-hardening-green.log` (SHA-256
  `196f3c56315f0b7ece5d2eb8f13477157de30c00027098ed15d1fab19d456509`).
  The final hashed lock has SHA-256
  `c7d75f89890f5522f2eab656e6d62890e72e3657de5c4604a78d1d22f9503f54`;
  the self-contained contract embeds the reviewed wheel filename/hash matrix,
  validates 3.11--3.14 coverage, inspects executable run scalars, and rejects
  missing/malformed/altered/unhashed/source-artifact or inert-command mutations.
- A read-only wheel proof resolved every one of the 18 pinned Python test
  dependencies for CPython 3.11, 3.12, 3.13, and 3.14 on
  `manylinux2014_x86_64`: 72/72 selections were binary wheels, with root
  METADATA name/version independently matched to the lock and no incompatible
  runtime/package pair. The machine record is
  `/tmp/codex/xenoteer-wheel-proof-agent-20260730/proof-manifest.json`
  (SHA-256
  `214454afd7c15b5afe8c6ccdc1d85e401739f86df3820e9146217d90e44cee55`).
  This proves the declared glibc-based Linux x86_64 CI matrix, not musl, ARM,
  macOS, or Windows.
- The final Python author gates are green: 85/85 unit tests, 73/73 immutable
  conformance cases, Ruff, strict mypy across 20 source files, 22/22 CI
  contracts, 16/16 package-boundary tests, fresh-venv hash-only/binary-only
  installation, and exact 25-file wheel/41-file sdist verification. Final wheel
  SHA-256 is
  `70b475332425fa5ec5d920b325c8d4b4403d218546563c8471e7ef75d28f8769`;
  sdist SHA-256 is
  `3f1b083b6c77c00f4e19b9bc415f5a67efa612f0a6ad9e42cf50f2114f1c6ebc`.
  The aggregate static script was not accepted as green in the author pass
  because its outer 10-second cap expired after its internal Python and package
  checks; the coordinator must run the complete gate without that undersized
  cap.
- The independent Python review found a remaining split-deadline defect in
  ordinary JSON and artifact HTTP paths: token resolution received one full
  timeout, then the injected/default HTTP adapter and body work received
  separate per-phase/full budgets. Only the additive
  `request_with_timeout` path had an outer timeout. Every public request path
  must instead use one absolute, client-cancellable deadline through final
  output semantics.
- Python WebSocket review found three adjacent medium gaps: failure cleanup,
  established sends/resubscribe/heartbeat, and old-socket retirement could
  await a blocking adapter without a bound; exported `EventSession.connect`
  still accepted and synchronously invoked credential callbacks on the event
  loop; and close classification used a permissive denylist instead of the
  exact cross-SDK transient set `{missing, 1001, 1012, 1013}`. These require
  blocked-adapter, zero-I/O sync-provider, and exact pre/post-welcome
  replacement-count regressions.
- Python post-repair review found close-once tracking stored `id(socket)` values
  forever. Since Python may reuse an object ID after the prior socket dies, a
  later reconnect/final socket could be mistaken for an already-closed socket
  and leak. The ownership design must be identity-safe with bounded lifetime and
  prove three or more reconnect generations under a deterministic collision.
- The new self-contained CI run-scalar parser still false-passed command-prefix
  impostors and failure masking because it used `startswith`: for example
  `npm test-fake`, `npm test || true`, or `npm test &` could satisfy the
  blocking `npm test` contract. The validator must require exact shell-token
  boundaries and reject background/control/masking syntax, with mutations for
  every required Rust, TypeScript, and Python command.
- Python distribution verification was also not truly Apache-only: it required
  an Apache marker and rejected BUSL specifically, but accepted another SPDX
  identifier such as GPL alongside the Apache marker. Every packaged text
  source must contain exactly the allowed Apache identifier and reject any
  second/non-Apache expression across both wheel and sdist mutation fixtures.
- Python follow-up RED evidence is now split deterministically. Absolute HTTP
  deadline RED
  `/tmp/codex/xenoteer-python-absolute-deadline-red.log` (SHA-256
  `869bdbeec87a3b1d66dd44d616a05aa8dd807292a25a543ba119ab29e88d7e28`)
  proves fresh auth/JSON/upload/download/delete budgets plus close/body stalls.
  WebSocket RED
  `/tmp/codex/xenoteer-python-websocket-bounds-red.log` (SHA-256
  `a391f541d79adb0e6ddbc4c3bbf86eb4e7bc546a1b1f44e4686f30319da60fc5`)
  proves the public sync-provider loophole and hanging failed-handshake cleanup
  and established send.
- The remaining Python/CI/package behavioral REDs are:
  `/tmp/codex/xenoteer-python-close-policy-red.log` (SHA-256
  `ab5441b1466747e8202394bb396a2325b18754aec007b05ff4c0680b9d74cad0`)
  for exact close/replacement behavior;
  `/tmp/codex/xenoteer-ci-shell-contract-red.log` (SHA-256
  `c33021db6106c3818eec51abac2c5e8fc25e8aaf14ce1dbc2e5f00d730269382`)
  for admitted command-prefix/failure-masking variants; and
  `/tmp/codex/xenoteer-python-spdx-red.log` (SHA-256
  `fda5f1840e2407e1aa6280718d7dc059838d816b5c542416cb4642fc48d28f7a`)
  with 19 accepted non-Apache/multiple-marker wheel and sdist mutations.
- The Python six-finding author repair is now green: one absolute HTTP/artifact
  deadline with active-operation close cancellation; bounded failed/established
  WebSocket cleanup, send, subscribe, resubscribe, heartbeat and retirement;
  async-only exported authorization; exact transient close set; shell-token-aware
  CI commands; and exactly one Apache SPDX marker per packaged source.
  Evidence includes 97/97 full tests (log SHA-256 prefix `fb82cbef`), 14/14
  focused WebSocket tests (`10debbf2`), 23/23 CI contracts (`327b3345`),
  73/73 conformance (`c2242dae`), strict mypy (`2d6b2fd3`), Ruff
  (`82b3e6a6`), and exact 25-file wheel/41-file sdist verification
  (`a762de78`). Independent post-repair review remains pending; final evidence
  must record complete hashes rather than these checkpoint prefixes.
- Post-repair archive probing found member-name collapse before validation:
  real wheel and sdist files with two `xenoteer/__init__.py` entries
  (non-Apache first, valid Apache second) were both accepted. Duplicate and
  normalization-alias members must be rejected before allowlist, metadata, or
  SPDX validation.
- Authoritative duplicate-archive RED is
  `/tmp/codex/xenoteer-python-package-duplicates-red.log` (SHA-256
  `3a97fa26f99ac1c095be304988881f47ca40059f9a787a92db8dba59f3e7ee08`):
  eight wheel/sdist bypasses cover malicious-first and identical-valid source,
  LICENSE, and metadata duplicates; malicious-last and normalization aliases
  are pinned as already-failing adjacent cases.
- Post-repair runtime probing also proved public synchronous artifact sinks can
  block the event loop beyond the absolute deadline; a sleep-based sink
  completed well past a 10 ms request timeout. Python cannot safely preempt
  arbitrary synchronous callback code, so exported download sinks must be
  genuinely async and validated before invocation or adapter I/O;
  `download_bytes` may retain an internal async collector.
- Artifact-sink RED
  `/tmp/codex/xenoteer-python-artifact-sink-red.log` (SHA-256
  `c34c86302d1a93dbbf835c70b5dd00c3f89c932fc4ac71518adf4b3bf80a136e`)
  proves both synchronous functions and callable objects execute/block and
  reach HTTP, including through exported `Artifacts.download_to`, rather than
  being rejected before I/O.
- Final Python review confirmed failed connect/negotiation cleanup awaited a
  client-owned injected transport's `close()` without an independent bound,
  even though ordinary client close is capped. Cooperative owned cleanup must
  be time-bounded, borrowed adapters untouched, and the original connect/
  negotiation failure must retain priority over cleanup timeout/failure.
- The same final review confirmed three adjacent medium gaps. Archive
  uniqueness was checked only before wheel dist-info/sdist root normalization,
  so alternate-version roots collapsed to the same logical allowlist and
  passed. A factory returning the same still-live socket in generation
  `A -> B -> A` created a fresh owner and closed A twice. The CI run parser
  treated required commands inside heredocs, uncalled functions, and
  `if false` bodies as executed. Repairs must validate normalized archive
  uniqueness, track live socket identity without raw-ID reuse or unbounded
  history, and require each gate as an exact simple executable step rather than
  attempting to infer arbitrary shell control flow.
- Final closure review found the simple-step validator ignored indented YAML
  plain-scalar continuation lines. A physically one-line accepted `run:`
  followed by a deeper-indented `|| true` is folded by YAML into one masked
  command while the validator saw only the first line. Required run scalars
  must reject all continuation content, not merely block-scalar syntax.
- Failed-connect cleanup RED is
  `/tmp/codex/xenoteer-python-connect-cleanup-red.log` (SHA-256
  `e6e67c3c9dd46b8917e97f05b6251561e765387a2259f558f66724516886d3bd`):
  malformed-status and caller-cancellation cases hang in owned `close()`, while
  throwing-close error priority and borrowed no-close already pass.
- Rust close classification now has explicit table coverage for 14 close-code
  rows across 28 initial/reconnect scenarios, including every transient member,
  normal/protocol/data/size/application terminal representatives, and exact
  auth/permission policy codes plus candidate counts. The focused log
  `/tmp/codex/xenoteer-rust-close-matrix-focused.log` has SHA-256
  `ac447ae88b3f83ab935c9ec59ee661ceff8425bffa15360de35c6d85edd4c388`.
- The coordinator's final Phase-6 source gate is green on the complete dirty
  candidate tree. The Rust workspace ran 1,344 passing tests with 24 native-only
  ignores and no failures; its log SHA-256 is
  `ee6b9604f43e875c93650ab12ed80591e04850b64ea9ecf77ed7807c63bb9349`.
  Warning-denied Clippy, warning-denied rustdoc, doctests, schema freshness,
  both standalone Rust fixtures, deterministic Cargo package boundaries, and
  cargo-deny also passed. Their respective evidence hashes are
  `fca00bdfeea4facd30c45b11fb0f2665ed94afcec93fbbeb2eb9fda77a09871c`,
  `2815b83af627571a7531928783e81d4d59aaff1aac5dd9e1f793bf43d4b2d617`,
  `782daac2e9082beb9a33f4e48412d82457557d5e62cdfe1d8999bef2bf2eaa04`,
  `89dbb400e013cb29ebd1df92b4387d2658687947e04953328b77da6e3bf445c4`,
  `ef1c5c125a90cc7d3e0981153172851073653d07d2ab8d20d73e502580fd8286`,
  `057260e02b0da9cb0de31a003161e67d075c0e10bcc7ce311a5e3586df2b5c10`,
  and
  `cee56e90ac485955b428c91f2f175de89f4b09256981d6b6a821571c2e18597f`.
- The final aggregate static gate passed after generated Python, mypy, Ruff,
  egg-info, and Node dependency caches were moved out of the source tree into
  `/tmp/codex`; its log SHA-256 is
  `43a66f6244c307193da4348392cad8a064d216b0efbd53d0ca07f51eaeaecee2`.
  Authenticated native X11, authenticated AT-SPI, and concurrent isolation
  gates all passed without reruns; their log hashes are
  `825c56b0faedfa7eac78c0ac1c7495eee482b83d6f92dd35b31b30950e088a12`,
  `324cb5a9ac5eeb73f0958f5129c46a983075ed06554b72024aba883c9afc3aab`,
  and
  `fa6d67f2f106c6bef0dc41a3212d1235753033183dd8f57dbd5a196451d4ebbe`.
- Source `e80825847909990f25c958715a3e165f7ef29d0a` produced clean production
  image
  `sha256:e594ed77422feb26cdbbb66464883e35536dbfae941bd35ad8120f7b61c6b201`
  and exact derived fixture
  `sha256:529fd865939e0a37c52c387ebb188a112aba857b2c74e588ac6b4efa765c3244`.
  Both record source tree
  `e344a7e1f966448b6327505729f4528d002f77a479514857535503952371df28`,
  dependency lock
  `b7db3b0586412ff866441664a8ce11eaee23cc3172f1d9733e41e3d5f2524151`,
  and `dirty=false`; the fixture records the exact production ID and preserves
  its 28-layer prefix in 32 total layers.
- The first Phase-5 AT-SPI live lane passed against that pair. The first
  production-lifecycle lane then failed at `test-image.sh:489` before executing
  viewer-denial assertions because the coordinator incorrectly held
  `/tmp/codex/xenoteer-heavy-build.lock` around the complete lane while nested
  `test-viewer-denial.sh` attempted the same non-reentrant lock for its Cargo
  fixture build. The nested acquisition expired after 120 seconds. Lanes 3-7
  were not started and lane 2 will not be rerun on this pair, so both identities
  are rejected despite no observed product assertion failure. Lane-1 evidence
  SHA-256 is
  `28ef8088296cea1f7fb32eeb47b42dab0f807125f2373f184f9bbba2321854d6`;
  rejected lane-2 evidence SHA-256 is
  `45cbc7f1b6c63bb6a6b905e493fed7fd7d20778ea885d422b33c5947bff0cced`.
- The first canonical-runner source candidate passed 45 focused runner
  contracts, shared identity/CI contracts, independent review, and the complete
  static gate, but the coordinator's real sudo-to-user host proof rejected its
  shared-lock mode: root normalized
  `/tmp/codex/xenoteer-heavy-build.lock` to `root:root 0644`, while the actual
  util-linux `flock PATH COMMAND` used by lane 7 opens the path read-write and
  failed for UID 1000 with status 66. Kernel/read-descriptor mock coverage had
  missed this CLI boundary. RED evidence is
  `/tmp/codex/xenoteer-phase6-shared-lock-real-red.log`, SHA-256
  `8ffee26eddfdbcb39a884900dc0470d9cbe7c7ef4a9a4efc642faa772b8b9a5d`.
- A group-DAC-only candidate also failed under Linux
  `fs.protected_regular=2`: util-linux `flock PATH COMMAND` adds `O_CREAT`, so
  an invoking user cannot open a differently owned regular file in the sticky
  shared parent even when its group mode is writable. That rejected candidate
  log has SHA-256
  `e494a0119ece67775081059e43764c090ea09a0b073aec66554ec77f111c160f`;
  the confirming `openat(... O_CREAT ...) = -EACCES` strace has SHA-256
  `4422ea9a25152efc7cffe07c4494f3736ec6a1e88d7f1af349d6959f7232e0c0`.
- The final shared-lock rule binds ownership to the already-open, verified
  sticky parent descriptor: the lock UID equals that parent's UID, its GID is
  the validated invoking account's primary GID, its mode is `0660`, and its
  link count must be exactly one before any ownership or mode normalization.
  The shared parent is never reowned. A freshly root-created parent therefore
  yields `root:invoking-group`, while an existing invoking-user-owned parent
  yields `user:user`; both root and the invoking user can use the same
  util-linux path lock. The private qualification-session lock remains
  `root:root 0600`, and all lock paths reject multiply linked inodes before
  mutation.
- The hardlink regression failed first for both shared and private locks; its
  RED log SHA-256 is
  `77a1f68470af011355accddf24bf7e39bdbd1a6979abec3b21462dc8b4e0671b`.
  The completed canonical runner suite passes 53/53 contracts with log
  SHA-256
  `ed2c3b833149518bacd6b5300baf5e783f4b2b5240c588540de58169c9e43647`.
  Its bounded real dual-parent proof passes exact owner/group/mode/link-count
  checks plus root and invoking-user path-flock acquisition for both variants;
  evidence SHA-256 is
  `986e1d4c73add64004dbfb743633981145dd181d6ac65ec0ba7ad93103336a5e`.
- Independent closure review reported no high- or medium-severity findings.
  Its separate 53/53 run has SHA-256
  `a43a0a5b9e04b9919968bac4b5046f845843268a3645fa4eb7ae298463d255d2`;
  its separate real `fs.protected_regular=2` proof additionally verifies
  session-lock privacy (`root` succeeds, invoking user is denied) and has
  SHA-256
  `e323d21afa1ba0fa3101beb95c144d3ddc64c0e38b8818ba1f71ad6046675f4d`.
  The complete post-repair static gate passes with log SHA-256
  `b1105509e2c7c0de9d729000afcaf81be26f07052fac550a42acb7cbc7186bb3`.
- Clean source commit `c1f5caf5b78fc993555fdcbacfe24b786e326035`
  produced production image
  `sha256:424a5e9e35f64c1f8cba24d70e0dad8ac4f9f72dd403662c8142311d3d48231e`
  and exact derived fixture
  `sha256:7c8ea2bd948f9905fa86c2c6ea7dd022507a8fa46030fd8bc3406f674032156d`.
  Both record clean source tree
  `8d67f0b46d785ee192e231c7b40dded063b3d38f8eab5e80393c6075976a610c`
  and dependency lock
  `b7db3b0586412ff866441664a8ce11eaee23cc3172f1d9733e41e3d5f2524151`.
  Production and fixture build logs have SHA-256
  `8d007f6ba364b16e511fb8864a71085f1c918ff87852ca6b78b9e0539c6fb903`
  and
  `d1f4ab52b8d83ce21efa14c48eceb6b0ceb8a60d3cb92c6d3977fa94c0b6bcf1`.
- The canonical first run against that pair passed lanes 1-4, then rejected
  lane 5 before runtime assertions. The noVNC spike Dockerfile received the raw
  production `sha256:<64hex>` as its `FROM`; BuildKit treated it as
  `docker.io/library/sha256:<digest>` and failed with registry pull
  `insufficient_scope`. The pair is permanently rejected and lanes 6-7 were
  not started. Lane hashes are respectively
  `28ef8088296cea1f7fb32eeb47b42dab0f807125f2373f184f9bbba2321854d6`,
  `049f7c4927d49e1ce9c29f4188540c85cd09bd0740fa69d1e92c12e5543c176c`,
  `8120aa31a608be5db7cd85384cf5ffc5bff6ff0107a8aa1c32b819a62cdb6ea7`,
  `9b3e5a18a0612733db14f37ca57d8245b0181b48d408805dc8e92831f59d08b1`,
  and rejected-lane
  `e3d32c78a6ec17e5715bbc6d8c4a1ce7ff003ac5cd74201f2191fe0e867d3d0d`.
  The rejected attempt manifest has SHA-256
  `5c8966201ec7275b8fb1a03ff59b482f9b948df1b47905b62e82c7a0bcf22d25`;
  canonical stdout has SHA-256
  `17d5789c21f573957efa27e34ce32c8999ffa07b97e1740dfea960bb41bfc435`.
- The lane-5 failure class reaches exactly three Dockerfile consumers:
  `scripts/container/test-novnc-spike.sh`,
  `scripts/container/test-browser-spike.sh`, and
  `scripts/container/build-desktop-app-fixture.sh`. They now share
  `scripts/container/local-image-build-reference.sh`: the helper admits an
  exact durable source, reserves a random owned alias and private mode-0700
  directory, requires the IID child path absent immediately before Docker,
  securely reads Docker's created IID child relative to the anchored
  directory, reduces safe umask-derived permissions to 0600, proves distinct
  full base-layer ancestry, freezes the derived ID for every downstream
  consumer, and cleans up or fails closed on signal and identity drift.
- The first real-Docker IID smoke rejected the pre-created-inode design after a
  successful build because current Docker/Buildx intentionally unlinks and
  recreates `--iidfile`. Its log SHA-256 is
  `78c0a40a6ca20f7017938706ce4d8a7c9f13f5c2804c19fbd27db113a1b689cc`;
  retained diagnostic tag `xenoteer:iid-smoke-3749016` has exact ID
  `sha256:58325d7ac5e9443d5c246e97cbfa14382711ecd95cdf1ddfeff7f4d0d4fe7b8b`
  and must not be deleted before the repair milestone is accepted.
- After the directory-anchored repair, focused fake-Docker coverage passes
  37/37 (log SHA-256
  `316af2eb3f6224c391c1c0b568ce18900a89e2c7d23ba86cd3a9700acde50d84`)
  and aggregate container Python coverage passes 132/132 (log SHA-256
  `36cad4d05cf1d8240eeebcd08e52040ebca0bcc04392ab5c5983b758c13901e0`).
  The locked real-Docker/default-builder smoke passes with exact derived ID
  `sha256:b9da3011a11eb546ced9b9e8e589b6c8f5ddb0b14ed25ce60ab4a7a287275b7c`
  and log SHA-256
  `763c71d7e9b06be25024fd9522fa1f9dbbe2b320cf35bf6b3153e793b816425c`.
- The complete low-priority static gate passes with SHA-256
  `5d31c63e84515af7eef86a3f1e854620ab1e9438cc65dc75368df81b455ea839`.
  To remain reliable under `nice 15` scheduling, exact mutation-protected
  ceilings are 60 seconds per packaged Cargo command, 90 seconds for the whole
  package verifier, 90 seconds for the local-image Python module, and 10
  seconds for each smaller container Python module. All Cargo-indirect gates
  remain serialized by `/tmp/codex/xenoteer-heavy-build.lock`, use
  `CARGO_BUILD_JOBS=2`, and run at `nice 15` with idle-class I/O.
- The first independent review of the directory-anchored repair found zero
  High findings and one Medium confined to the test harness: the deliberate
  reservation-owner-record mismatch correctly makes production cleanup fail
  closed, but the Python regression did not remove its exact test-created
  reservation afterward. Repeated focused runs therefore left a mode-0700
  directory plus its mode-0644 IID child in `/tmp`. The correction must remain
  parent-side and exact-path, validate the residue before removal, run even
  when an assertion fails, and must not weaken the production helper or use a
  glob.
- The test-harness residue correction records the exact random reservation
  before corrupting the helper's recorded UID, then registers an idempotent
  parent-side cleanup before its production-behavior assertions. That cleanup
  accepts only the exact `/tmp/xenoteer-local-image-<32 lowercase hex>` path,
  anchors the current-UID mode-0700 directory without following links, accepts
  only the expected regular current-UID single-link mode-0644 IID containing
  the exact derived ID, removes it relative to the open directory, verifies
  unchanged identity and emptiness, removes that exact directory, and proves
  absence. RED comparison SHA-256
  `1549c03a666bf28b839bc5ab1a5e433073011a01330a3068147c2a305f40c028`
  proved the original test added one residue; the corrected targeted and
  timeout-contract log SHA-256 is
  `f8fc95c21927b189efb4785e9605fff5cecdd037ad21cc1b170f7df80e3d79d7`,
  aggregate container coverage passes 132/132 with log SHA-256
  `87c83373c6ea41bc0a2cc7ea63496ffa1764e45f2ec5bc8e945fe7cff9143664`,
  and CI contracts pass 31/31 with log SHA-256
  `6e3250de04059e6f43aee3698ab0476b10738aea63c54b2818728dd898223943`.
  A separate required-low-priority focused run took 42.985
  seconds, so the local-image module's mutation-protected aggregate watchdog is
  90 seconds; every child command remains independently bounded to eight
  seconds.
- The fresh independent review of the corrected 12-file snapshot reports zero
  High and zero Medium findings. Its separate targeted owner-mismatch run
  passed 1/1 while leaving the exact 15-path pre-test residue inventory
  unchanged, and its independent updated CI-contract run passed 31/31. The
  remaining lower-severity observations are accepted for this boundary:
  inspect metadata capture is not independently size-bounded; cleanup has an
  inspect-to-remove concurrency window against unrelated same-Docker writers;
  cleanup inspect/remove calls have no separate local watchdog or complete
  signal coverage; TERM grace is about 200 milliseconds; and production
  intentionally retains a private reservation when its provenance can no
  longer be proven.
- The complete post-review, post-timeout-calibration static gate passes under
  the shared heavy-build lock, `nice 15`, idle-class I/O, two Cargo jobs, and
  two Rust test threads; its log SHA-256 is
  `cf41ec7fe1fb1dc41b6912c408d022f52d4e34f73f76535a315342456732f121`.
  Fifteen exact stale `/tmp/xenoteer-local-image-<nonce>` reservations created
  by earlier pre-fix adverse regression runs were individually validated and
  removed without globs. No reservation remains. Retained diagnostic image
  `xenoteer:iid-smoke-3749016` remains untouched at
  `sha256:58325d7ac5e9443d5c246e97cbfa14382711ecd95cdf1ddfeff7f4d0d4fe7b8b`.
- Clean source commit `439d45e1d736ecf3e657e2625207ee4215780cbe`
  produced production image
  `sha256:0c50f668030caaa403668efd619a82d1c3af17deb2faf3b25e63658ae60747b7`
  and exact derived fixture
  `sha256:0784d251d85c4240f2a24e5dc5b9464daa648afb1c0936320ae4b6b7e9ab8b9b`.
  Both record clean source tree
  `e4855a78e8534cd4e85bb5281ee4dfc3245a0d1e1ff3035fd3226add24f19525`
  and dependency lock
  `b7db3b0586412ff866441664a8ce11eaee23cc3172f1d9733e41e3d5f2524151`.
  Production and fixture build logs have SHA-256
  `a552628eeb0f97be23f74bd861ded07cbfc60c9248a2530b065fe1e6153332f5`
  and
  `d6eeda88fa2e58e7ddf7f53549fdc3184ec8607bca6aff31a2219e37c52e76c2`.
- That pair is permanently rejected after its only canonical qualification
  attempt. Lanes 1-6 passed: Phase-5 AT-SPI live, production lifecycle,
  Phase-4 live fixtures, Phase-4 event flood, exact-ID noVNC, and the desktop
  application matrix. Their log SHA-256 values are respectively
  `28ef8088296cea1f7fb32eeb47b42dab0f807125f2373f184f9bbba2321854d6`,
  `47629c46fbbfa7cf7f69a6a1da2706ff9cf8624193d72fd338672be631cd1fb7`,
  `8120aa31a608be5db7cd85384cf5ffc5bff6ff0107a8aa1c32b819a62cdb6ea7`,
  `a816cd4e817e6e23760e90d174fbfad28bfa253e4ae4eb557d02373c71f14b1e`,
  `83f7908c73b03e93943365b6234bf5366ac36fe8b70aea06dac434e1ea3e9533`,
  and
  `521218f5a16f0e54b03712376603a7e9110ca1c1d71426546da46f2c2746be43`.
  Lane 7 rejected in 1.787 seconds with
  `required package build executable is unavailable: npm`; its log SHA-256 is
  `45c7e3e9be0a69d10a4cff6832a7c61313d2d92d7569e393908ffc44a3606ad0`.
  Attempt-manifest SHA-256 is
  `2a70d194ce729b98e40594794f49dfce728cde459467af07f29787e05ed6948a`
  and canonical stdout SHA-256 is
  `62a3cddd3896e917ea1b4ebf6e3a3318383f13f1b1bdbf8c7c428cd967434aac`.
- The rejected source's immediate cause is deterministic: its root runner's
  `_package_tool_path` produced only
  `/home/wyatt/.cargo/bin:/usr/sbin:/usr/bin:/sbin:/bin`, while this host's
  supported Node 24/npm installation exists only at
  `/home/wyatt/.nvm/versions/node/v24.18.0/bin`. The fix must not source
  user-controlled shell initialization or forward the caller's ambient
  `PATH`; it must preserve the existing post-sudo ownership, primary-group,
  traversal, symlink-target, executable, and clean-environment validation.
- The corrected lane-7 boundary discovers supported Node 22/24 installations
  without executing user shell initialization or forwarding the caller's
  ambient environment. The qualifier performs a deterministic, filesystem-only
  scan of at most 64 NVM version entries, otherwise admits only a coherent
  same-directory root-owned system pair, and passes the result solely through
  `XENOTEER_PACKAGE_BUILD_PATH`. Root Docker/Git operations retain the fixed
  `/usr/sbin:/usr/bin:/sbin:/bin` environment.
- The public package runner consumes only the first exact selected path entry,
  rejects aliases and replacement fallback, validates a canonical regular NVM
  `node` plus the in-root `npm` symlink target component-by-component with
  no-follow descriptors, and requires the exact bounded
  `#!/usr/bin/env node` wrapper. It then runs one output-, time-, and
  process-group-bounded version probe after dropping to the invoking identity
  and threads the same immutable toolchain through staging, installation, and
  live execution.
- Coordinator focused evidence is public quick-starts 70/70, qualifier 72/72,
  and CI contract 33/33. Their log SHA-256 values are respectively
  `1b58a876f931ad5c885161232890cc6c3cd70d1ecde108f9e9c199d1a2fa4c7e`,
  `51d032514a2f4a33d006bdfa4d567fdd31ebf08ba42f5b56bbbd1767946987c7`,
  and
  `c5cb0cd52790b4159711a56a0f0cdaea819127a718ed847b451c56f975819c0c`.
  The complete low-priority static gate also passes; its log SHA-256 is
  `a9a1e7e773412694365e6fb4a7d5ac82fa0921c51690a3122ad9ec2f5a2ba861`.
- A real root-to-invoking-user proof ran with a deliberately hostile root
  `PATH`, retained the fixed system-only root command boundary, selected exact
  Node 24.18.0/npm 12.0.1 from the invoking user's NVM tree, and executed the
  package tools as UID/GID 1000. Its log SHA-256 is
  `b5ad21e9d415731f5da4079ef7e338f6b613c062609ebe1dccc6ec8a217b65f9`.
  The fresh independent frozen-source review reports zero High, zero Medium,
  and zero Low findings after independently passing the same 70/72/33 focused
  suites and verifying every supplied source hash before and after review.
- Clean source commit `eb743701ad37ba40548d07edfad2cc6b1c737e3a`
  produced production image
  `sha256:47f986ed974560b62f73c0dde2ff44bf9f3aa22699e35bdca19be295e2f61dc2`
  and exact derived fixture
  `sha256:883069425841de22aaa893e6be6355a2cb24dc3c08bd04c2e398b1c9e90424b3`.
  Both record clean source tree
  `2e8472be6114840794067f2a3b77ef4013effad111ae9e622faeb2fe2ca8d9be`
  and dependency lock
  `b7db3b0586412ff866441664a8ce11eaee23cc3172f1d9733e41e3d5f2524151`.
  Production and fixture build logs have SHA-256
  `4da4bfb17a548deac1090f093b8fa77d22efd4ab8c16e21451cc55a15a9e8e09`
  and
  `0da80243bde119e67cb3f37fe39c60e4325cb770ff2c166a1e4f000909527e5b`.
- That pair is permanently rejected after its only canonical qualification
  attempt. Lanes 1-6 passed: Phase-5 AT-SPI live, production lifecycle,
  Phase-4 live fixtures, Phase-4 event flood, exact-ID noVNC, and the desktop
  application matrix. Their log SHA-256 values are respectively
  `28ef8088296cea1f7fb32eeb47b42dab0f807125f2373f184f9bbba2321854d6`,
  `78d2622a131ac09685f9d414e35585a6771d77adb1ca3f546070cfdad595708a`,
  `8120aa31a608be5db7cd85384cf5ffc5bff6ff0107a8aa1c32b819a62cdb6ea7`,
  `92d252f7f37bf703a6f8a8f2f6fa01f265f695567970c7b84e1fee4338dd3a19`,
  `42aff3a319a94d96a44d76b6442c1eeea770a10b24baa8b533fadf2a8bb1247f`,
  and
  `4b82abb7b286f68dffbe72eb477c2614b72b46274815d3c019079f039b6bab1a`.
  Lane 7 rejected in 629 milliseconds before Docker image inspection with
  Git's user-owned-checkout `dubious ownership` error while running
  `git rev-parse --verify HEAD`; its log SHA-256 is
  `7bc9570cc2406ced7a9d40658e1f306b03aa82ac7cb9315af5784b8e7da58ef8`.
  Attempt-manifest SHA-256 is
  `9eebaa41ab66b9694cec398f3640e5a7e746ddef889615d5359cbabbceaedf78`.
  The repair must execute repository Git identity/hash reads as the validated
  invoking user and retain the fixed root/system environment for Docker; a
  root-global or per-command `safe.directory` exception is not acceptable.
- The accepted Git source-identity repair resolves one root-owned Git
  executable from the fixed system path and runs only the four source-tree Git
  fences as the validated invoking UID/primary GID with no supplemental groups
  and an exact six-key password-database environment. Docker remains on the
  root fixed-system executor. The final frozen source hashes are runner
  `1ae7ae2b540596d26269a5284ccbc9ca210b573e346b2c9130425b7353c97d58`,
  tests
  `6b993480477594f5dc2350df2781c2cb3732c7302b4182d279ba7c66c77804fa`,
  and README
  `85d31658d15aed636b6f9e6740de055de78d5ad3084f9af0f618aa318dee8032`.
  A test-only UID-0 portability defect failed first with log SHA-256
  `3afec80ab35a44284b53d844e86585bcc43f685d1170741322d540e572b325b3`
  and was corrected without weakening the production root rejection.
  Independent corrected-snapshot review reports zero High, Medium, or Low
  findings; its ordinary and UID-0 targeted tests each pass 2/2.
- Coordinator post-review evidence passes public quick-start 77/77, shared
  identity 8/8, CI contract 33/33, and qualifier 72/72 with combined log
  SHA-256
  `ca95094223dd2070d20db1950d49789d6183f9c91c74496447caa34dccfdfe54`.
  The complete low-priority static gate passes under the shared heavy-build
  lock, two Cargo jobs, two Rust test threads, `nice 15`, and idle-class I/O;
  its log SHA-256 is
  `8df2d5806d533cd2e97d41abfe8d1544168dd781394368cebfc6e75a8f0f0313`.
  No generated Python cache remains. The next valid action is to commit/push
  this repair, build exactly one new coherent production/fixture pair from that
  clean commit, and run the canonical seven lanes exactly once.
- The read-only Phase-7.1 transport design artifact is
  `/tmp/codex/xenoteer-phase7-wave1-design.md`, SHA-256
  `414641146e2cddeb7edd6855c3f3b1c75410a9614fb5e9522d8b6615970960b6`.
  It is **not implementation-ready** despite its internal status line. Fresh
  adversarial review found that Hyper's proposed 10-second header timer can
  overlap and truncate a still-streaming response, loopback TCP is reachable
  by the explicitly untrusted desktop UID and therefore is not reserved
  operations capacity, the end-to-end transport/actor/supervisor shutdown
  budget is not reconciled against actual container supervision ceilings, its
  raw-head tests omit Hyper's historical segmented-write `max_buf_size`
  bypass, and its `-j 4` commands violate the current strict two-job resource
  ceiling. Correct and independently re-review the design before Phase 7.1
  implementation.
