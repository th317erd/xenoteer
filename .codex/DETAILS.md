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
  inspection; Python 38/38 tests, Ruff, mypy, wheel/sdist inspection, and the
  deterministic event-completion barrier; the 13/13 CI-contract tests; and the
  static mirror, first-party license, and cargo-deny gates.
- Phase-6 public quick-start qualification must install the staged crate/npm/
  wheel artifacts rather than importing source trees, run every language
  against the same immutable release-candidate image ID, cover bounded
  invalid-auth/failure cleanup as well as success, and reject daemon binary
  overrides.
- The executable Phase-6 gate is
  `scripts/sdk/test-public-quickstarts.py`. It reproduces the Docker build
  wrapper's source-tree identity, requires it to equal the image label, resolves
  and runs only one immutable image ID, safely extracts both Rust archives,
  installs the npm tarball plus Python wheel and sdist into isolated roots, and
  runs typed invalid-auth and successful v1 status probes for every variant.
  Its unit, archive-install, and bounded mock-runtime proofs pass. No Docker
  image was built during final closure, so the exact-image execution and
  identity record intentionally remain pending until the coherent Phase-6 image
  is built.
- Phase-7 transport hardening must operate before Axum/Tower request middleware:
  the accepted-connection permit, header count/bytes/read deadline, keep-alive
  idle bound, incomplete-request bound, and reserved health/shutdown capacity
  must be proven with raw sockets. Router-level body/rate tests are not evidence
  for this boundary.
- Phase-7 runtime authentication must add atomic complete token-set reload,
  metadata/expiry/revocation and scoped principals, including defined closure or
  reauthorization of already-active WebSockets. The existing multi-record auth
  library alone is not runtime rotation evidence.
- Phase-7 environmental release observations that cannot be honestly reproduced
  locally include protected GitHub environments/tags, OIDC registry signing,
  public-registry clean-host verification, a genuine 24-hour active soak, and
  the supported-host LSM matrix. Implement their harnesses/workflows locally but
  record those external observations as unverified until executed there.
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
