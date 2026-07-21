# Rust code design: workspace, modules, types, and state machines

## 1. Workspace shape

Use one Cargo workspace with resolver 3 and the current stable Rust edition at
implementation start. Commit `rust-toolchain.toml` and `Cargo.lock`. Set a
documented MSRV after the first release; until then the pinned toolchain is the
only supported compiler.

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
deny.toml
crates/
  xenoteer-protocol/
  xenoteer-core/
  xenoteer-x11/
  xenoteer-atspi/
  xenoteerd/
  xenoteerctl/
  xenoteer-sdk/
test-apps/
  x11-event-recorder/
  gtk-fixture/
  qt-fixture/
  web-fixture/
tests/
  e2e/
fuzz/
schemas/
```

Do not create one crate per actor. These seven boundaries isolate public schema,
domain logic, platform backends, executable composition, CLI, and SDK without
microcrate overhead.

## 2. Workspace dependency policy

Declare shared versions/features under `[workspace.dependencies]`; member crates
use `workspace = true`. Phase 0 locks a mutually compatible set from current
stable releases and records rationale in `Cargo.lock`/dependency manifest.

Expected direct dependencies:

| Concern | Dependency policy |
|---|---|
| Async/runtime | Tokio with only used features; `tokio-util` cancellation/task tracking |
| HTTP/WS | Axum, tower, tower-http with explicit middleware features |
| Serialization/schema | serde, serde_json, schemars; TOML for config |
| IDs/time | uuid with v4/v7+serde; time crate with formatting/serde |
| Errors | thiserror in libraries; anyhow only at executable/diagnostic boundaries |
| X11 | x11rb pure Rust, selected features only: xtest, xkb, xfixes, damage, shm, composite, randr, image |
| Keyboard | xkbcommon binding matching Debian libxkbcommon |
| Accessibility | atspi with proxies/connection/tokio-compatible features; zbus version it supports |
| Images | png plus a small tested resize library or narrowly enabled image features |
| CLI | clap derive, reqwest/rustls only in CLI/SDK |
| Diagnostics | tracing, tracing-subscriber, metrics/exporter |
| Security | secrecy, subtle/constant-time token comparison, sha2/hmac only as required |
| Algorithms | regex, indexmap, lru or an internally wrapped bounded cache |
| Testing | proptest, insta only for normalized text/schema snapshots, tempfile |

Default features are disabled on large crates where they pull unused runtimes,
TLS stacks, formats, or native libraries. `cargo tree -e features` is reviewed.
Use rustls for client/server TLS if daemon TLS is implemented; do not ship two
TLS stacks accidentally.

All direct dependencies need license, maintenance, advisory, and transitive
review under [16](16-dependencies-and-licensing.md). `cargo-deny` enforces allowed
licenses, duplicate/version policy, advisories, and approved git sources.

## 3. `xenoteer-protocol`

### 3.1 Boundary

Pure serializable wire model. It MUST compile on non-Linux hosts without X11 or
D-Bus libraries so TypeScript/Python generators and SDK tests can consume it.
No Axum extractors or backend types.

```text
src/
  lib.rs
  version.rs
  ids.rs
  capabilities.rs
  envelope.rs
  commands/
    mod.rs mouse.rs keyboard.rs text.rs window.rs element.rs
    clipboard.rs application.rs capture.rs lease.rs
  snapshots/
    desktop.rs window.rs element.rs process.rs selection.rs
  events.rs
  errors.rs
  pagination.rs
  validation.rs        # shape-only constants/helpers, no backend calls
```

### 3.2 Serialization rules

- Externally tagged command/event `type` strings in `snake_case`, stable once
  released.
- `#[serde(deny_unknown_fields)]` on command/request structs. Responses may be
  additive, and SDKs preserve/ignore unknown fields according to version policy.
- Integers for coordinates/durations/byte counts; no floating duration strings.
- RFC 3339 UTC timestamps for external time and integer monotonic durations for
  measurements.
- UUID text for IDs. `CommandId` is caller-generated UUID v4/v7; no arbitrary
  unbounded strings.
- Enums serialize to names, never Rust discriminants.
- Optional absence and explicit null have one documented meaning per field.
- Binary data is not base64 inside ordinary WebSocket JSON when it can be an
  authenticated artifact/HTTP body; clipboard small bytes may use bounded base64
  with explicit encoding.

Generate JSON Schema from these types and compare checked-in `schemas/v1/*.json`
in CI. Schema generation is deterministic.

### 3.3 Core protocol types

```rust
pub struct DesktopId(pub Uuid);
pub struct DesktopGeneration(pub Uuid);
pub struct CommandId(pub Uuid);
pub struct RequestId(pub Uuid);
pub struct ControlLeaseId(pub Uuid);
pub struct ArtifactId(pub Uuid);

pub struct Point { pub x: i32, pub y: i32 }
pub struct Size { pub width: u32, pub height: u32 }
pub struct Rect { pub x: i32, pub y: i32, pub width: u32, pub height: u32 }
pub enum CoordinateSpace { RootPhysical, WindowClient, WindowFrame, AtspiScreen }
```

Keep fields private only if serde/schema construction remains ergonomic; provide
checked constructors in core. Protocol deserialization can create unchecked
shapes that admission validation converts into domain types.

### 3.4 Envelopes

```rust
pub struct CommandEnvelope {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub command_id: CommandId,
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub lease_id: Option<ControlLeaseId>,
    pub deadline: Option<OffsetDateTime>,
    pub trace_policy: Option<TracePolicy>,
    pub command: Command,
}

pub struct CommandResult {
    pub command_id: CommandId,
    pub lifecycle: CommandLifecycle,
    pub effect_stage: EffectStage,
    pub accepted_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
    pub outcome: Option<CommandOutcome>,
    pub error: Option<Problem>,
    pub warnings: Vec<Warning>,
}
```

`CommandResult` invariant: exactly one of terminal outcome/error when lifecycle
is terminal; neither while accepted/running. Enforce with constructors and tests,
not public struct literals in application code.

## 4. `xenoteer-core`

### 4.1 Module tree

```text
src/
  lib.rs
  domain/
    ids.rs geometry.rs references.rs capabilities.rs errors.rs
  action/
    coordinator.rs command_record.rs effect.rs deadline.rs dispatcher.rs
  lease/
    state.rs policy.rs
  input/
    action.rs state.rs interpolation.rs keyboard_plan.rs cleanup.rs
  selection/
    model.rs transfer.rs
  window/
    model.rs selector.rs wait.rs correlation.rs
  accessibility/
    model.rs selector.rs wait.rs correlation.rs
  process/
    model.rs launch_policy.rs
  observation/
    event.rs sequence.rs snapshot.rs
  supervisor/
    state.rs capability_probe.rs
  config/
    model.rs validate.rs redact.rs
  ports.rs
```

Core contains pure state machines and normalized domain models. It may depend on
Tokio synchronization/time types at application seams, but pure algorithms do
not require a runtime in tests.

### 4.2 Checked domain geometry

Use newtypes to prevent coordinate confusion:

```rust
pub struct RootPoint(Point);
pub struct ClientPoint(Point);
pub struct ScreenRect(Rect);

impl ScreenRect {
    pub fn contains(&self, p: RootPoint) -> bool;
    pub fn intersect(&self, other: &Self) -> Option<Self>;
}
```

Conversions require context (`WindowGeometry`, `AtspiCoordinateSpace`) and return
`Result`; do not implement ambiguous `From<Point>` conversions. Checked addition
for relative moves rejects i32 overflow before screen validation.

### 4.3 References

```rust
pub struct WindowRef {
    pub desktop: DesktopScope,
    pub xid: u32,
    pub observed_generation: u64,
    pub identity: IdentityHash,
}

pub struct ElementRef {
    pub desktop: DesktopScope,
    pub atspi_generation: u64,
    pub app: ApplicationRef,
    pub object_path: Arc<str>,
    pub identity: IdentityHash,
    pub cache_sequence: u64,
}
```

`IdentityHash` serializes to fixed lowercase hex/base64url and is computed with
a stable domain-separated hash. It is not a secret or authorization token.

### 4.4 Error type

Library variants are structured with thiserror. Map all adapter errors into:

```rust
pub struct Error {
    pub code: ErrorCode,
    pub operation: &'static str,
    pub retry: RetryAdvice,
    pub effect: EffectJournal,
    pub safe_details: BoundedDetails,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}
```

`Debug` may include source under server logging policy; protocol conversion uses
only safe fields. No `.to_string()` of arbitrary backend error is sent to clients.

### 4.5 Coordinator actor

Sole owner of lease and command ledger:

```rust
enum CoordinatorMessage {
    Submit { principal: Principal, envelope: CommandEnvelope, reply: oneshot::Sender<SubmitReply> },
    RenewLease { ... },
    ReleaseLease { ... },
    Cancel { command_id: CommandId, ... },
    DesktopStateChanged(DesktopStateSnapshot),
    Shutdown { reply: oneshot::Sender<()> },
}
```

`CoordinatorTask` selects over:

- bounded message receiver;
- `FuturesUnordered`/JoinSet of accepted executions;
- lease expiry timer;
- desktop state watch;
- shutdown token.

It never awaits one command to completion in the message branch. On acceptance,
it inserts `CommandRecord`, creates a `watch::Sender<CommandResult>`, and spawns a
tracked execution future under global/per-principal semaphores. Completion is
polled and atomically recorded. Duplicate submit returns the existing watch
receiver after principal/request-hash checks.

`CommandRecord` state transitions are closed:

```text
Accepted -> Running -> Succeeded
                    -> Failed
                    -> CancelledAfterEffect
Accepted -> CancelledBeforeEffect | DeadlineBeforeEffect
Running  -> DeadlineAfterEffect
```

Terminal state cannot change. Property tests generate transition sequences.

### 4.6 Execution dispatcher/sagas

Dispatcher matches the domain `Command` and calls typed handles. Multi-system
operations are explicit functions, e.g. `execute_element_click`:

```text
validate element -> correlate/revalidate window -> activate window
-> refresh extents -> queue input -> wait postcondition -> assemble journal
```

Each step appends effect evidence. Cleanup errors are secondary structured fields
without replacing the primary error. Sagas do not implement generic rollback.

## 5. `xenoteer-x11`

### 5.1 Module tree

```text
src/
  lib.rs
  connect.rs
  atoms.rs
  extensions.rs
  error.rs
  barrier.rs
  input/
    actor.rs handle.rs command.rs pointer.rs button.rs keyboard.rs reset.rs
  observe/
    actor.rs handle.rs events.rs model_update.rs properties.rs tree.rs
  window/
    ewmh.rs icccm.rs geometry.rs selector_data.rs
  clipboard/
    actor.rs owner.rs requestor.rs incr.rs targets.rs
  capture/
    actor.rs get_image.rs pixel.rs cursor.rs shm.rs png.rs
  test_support.rs
```

Only this crate imports x11rb/xkbcommon. Enable the minimum x11rb extension
features explicitly; do not use `all-extensions` in production.

### 5.2 Connection creation

`connect::open(role, config)`:

- uses explicit DISPLAY/Xauthority inputs, not ambient guessing;
- verifies selected screen and setup bounds;
- queries required/optional extension versions;
- returns `XConnectionInfo` with root, visual, formats, keycode range, extension
  opcodes, and connection generation;
- applies maximum request/reply expectations;
- labels every error with connection role.

Connections are never cloned/shared between roles.

### 5.3 Input actor messages

```rust
pub struct InputCommand {
    pub context: ActionContext,
    pub operation: InputOperation,
    pub cancellation: CancellationToken,
    pub public_queue_permit: Option<OwnedSemaphorePermit>,
    pub reply: oneshot::Sender<Result<InputOutcome>>,
}

pub enum InputOperation {
    Move(MoveAction), Click(ClickAction), Drag(DragAction), Scroll(ScrollAction),
    Key(KeyAction), TextPhysical(TextPhysicalAction), Reset(ResetReason), Probe,
}
```

Use `tokio::sync::mpsc` with physical capacity `PUBLIC_CAPACITY + 1`. Public sends
must carry a permit from a separate semaphore capped at `PUBLIC_CAPACITY`; the
one reserved channel slot allows a coalesced reset/shutdown control command.
The actor thread calls `blocking_recv`. Before each command it drains pending X
events/errors (including mapping changes), refreshes required model state, then
executes. A periodic supervisor Probe wakes an otherwise idle actor and detects
connection loss.

Thread creation returns `InputHandle` and a join-report channel. Wrap the run
entry in `catch_unwind`; a panic sends a critical actor failure and triggers
daemon shutdown. Do not resume normal input after panic.

### 5.4 Barrier

`barrier::query_pointer(&conn, root)` sends QueryPointer, flushes, waits for the
reply, and associates any protocol error with the active command/effect stage.
Use reply-producing requests instead of `flush()` as proof of processing;
`flush()` only writes buffered bytes.

### 5.5 Interpolation implementation

Core produces `Vec<MotionSample { point, delay_ms }>` under bounded count. X11
adapter only validates conversion to `i16`, sends one XTEST request per sample,
then barrier. Keep math out of the adapter for property testing.

Avoid sleeping the host thread for movement timing when XTEST delay is used.
Host-side timing would add scheduler jitter and does not create the same
same-connection barrier semantics.

### 5.6 Observation event loop

Preferred Unix implementation wraps the connection file descriptor in Tokio
`AsyncFd` under a single owner task using x11rb's event-loop integration feature:

1. await readability;
2. drain `poll_for_event` until empty;
3. clear readiness and retry correctly for edge-trigger races;
4. normalize/update model without awaiting slow subscribers;
5. service bounded command messages in `select!`;
6. batch/coalesce property refreshes.

If the pinned x11rb `RustConnection` cannot satisfy `AsyncFd` safely, implement a
dedicated poll thread with an explicit wake FD. Do not fall back to a sleep-poll
loop or shared mutex. Phase 0's connection spike decides only this mechanical
adapter detail; actor APIs and ownership remain fixed.

### 5.7 Clipboard loop

Use the same single-owner readiness pattern because selection protocol requires
prompt X event handling while daemon commands arrive. `IncrTransfer` is a keyed
state machine:

```text
Offered -> WaitingForDelete -> SendingChunks -> SendingTerminator -> Complete
                         \-> TimedOut | RequestorGone | ProtocolFailed
```

Timers are maintained by actor task, with total/global transfer limits. Unit
tests operate the state machine without X server.

### 5.8 Capture actor

Dedicated blocking thread with bounded MPSC is acceptable because capture is
request/reply, not an unsolicited event loop. Core GetImage reply is decoded by
pure `pixel` functions, then encoding runs under the daemon's bounded blocking
executor or an internal fixed worker pool. SHM resources have RAII wrappers for
local detach plus explicit server detach barrier; RAII alone is not considered
server cleanup proof.

## 6. `xenoteer-atspi`

```text
src/
  lib.rs
  actor.rs
  connect.rs
  error.rs
  cache/
    mod.rs item.rs compatibility.rs reconcile.rs limits.rs
  model/
    normalize.rs roles.rs states.rs interfaces.rs geometry.rs
  query/
    selector.rs traversal.rs
  action/
    invoke.rs focus.rs value.rs selection.rs text.rs scroll.rs
  events/
    subscribe.rs normalize.rs coalesce.rs
  correlation.rs
```

Use atspi proxies/connection APIs, not hand-built raw D-Bus messages unless a
documented compatibility case is missing from the crate. Enable zbus Tokio and
disable unused default runtime features to avoid two async executors.

`AtspiHandle` exposes typed async methods and watch/event receivers; callers do
not hold proxies. The actor/cache owns bus names and object paths.

Cache keys use owned unique bus name + owned object path + application instance
generation. Strings from D-Bus are length-checked during normalization. Selector
traversal carries a `QueryBudget` counter on every node/proxy call/byte.

Event subscription registration is explicit and reversed at shutdown where the
protocol supports it. Stream errors trigger generation/rebuild, not an ignored
background task exit.

## 7. `xenoteerd`

### 7.1 Module tree

```text
src/
  main.rs
  app.rs
  startup.rs
  shutdown.rs
  config.rs
  state.rs
  supervisor.rs
  api/
    mod.rs router.rs middleware.rs extract.rs problem.rs
    health.rs status.rs commands.rs results.rs events.rs viewer.rs artifacts.rs
  auth/
    mod.rs principal.rs static_token.rs policy.rs
  process/
    manager.rs launch.rs terminate.rs output.rs reference.rs
  viewer/
    supervisor.rs gateway.rs ticket.rs
  artifacts/
    store.rs filesystem.rs retention.rs
  telemetry/
    logging.rs metrics.rs redaction.rs
```

### 7.2 Startup order

1. parse CLI and config file/env overlays;
2. validate full config and log redacted effective summary;
3. initialize tracing/panic hook/metrics;
4. establish desktop identity/generation from supervisor environment;
5. create X input/observation/clipboard/capture actors;
6. connect AT-SPI actor;
7. create process/artifact/viewer handles;
8. start coordinator and event hub;
9. run capability probes and update readiness;
10. bind listeners last, or bind early but return not-ready until step 9;
11. notify s6 readiness only after API listener and required capabilities work.

Startup failure unwinds/join-stops already-created actors. Avoid `std::process::exit`
below `main`; return errors so destructors/join logic can run.

### 7.3 AppState

```rust
#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    coordinator: CoordinatorHandle,
    supervisor: SupervisorHandle,
    events: EventHubHandle,
    artifacts: ArtifactStoreHandle,
    viewer: ViewerHandle,
    metrics: MetricsHandle,
}
```

No raw actor connection or mutable model is present.

### 7.4 API tasks

REST handlers perform bounded extraction/auth and delegate. WebSocket handler
splits socket once into exactly one reader and writer task:

- reader parses bounded UTF-8 JSON and submits/cancels/subscribes;
- writer owns sink and selects high/normal outbound queues, ping timer, shutdown;
- session supervisor joins both; either terminal side cancels the other;
- command execution survives transient WS disconnect according to command/lease
  policy, and its result remains in ledger.

Do not spawn a writer per command. Do not serialize giant snapshots on the
reader/writer task.

### 7.5 Process manager

`ProcessManager` owns every launched child handle and process group. Public
reference:

```text
ProcessRef { desktop generation, pid, proc_start_ticks, launch_id }
```

Launch uses `tokio::process::Command` with argv array, `env_clear`, explicit env,
cwd validation, stdin policy, bounded stdout/stderr readers, and Unix
`process_group(0)`. `kill_on_drop` is defense in depth, not lifecycle control.

Manager task awaits/reaps all children and emits exit events. Terminate sends
SIGTERM to verified group, waits configured grace, then SIGKILL and awaits.
Before signaling, revalidate PID/start-time/group identity to avoid PID reuse.
No shell unless an entirely separate authorized command explicitly launches the
shell binary as argv[0].

## 8. `xenoteer-sdk`

Rust SDK owns transport/reconnect/result handles, not server internals:

```text
Client -> Desktop -> Mouse/Keyboard/Clipboard/Windows/Accessibility/Apps
CommandHandle<T> -> accepted/running/result/cancel
Window -> generation-bound WindowRef
Element -> generation-bound ElementRef
EventStream -> sequence/resync helpers
```

Use reqwest/rustls for REST/artifacts and a maintained WebSocket client compatible
with server framing. Centralize reconnect; commands with terminal/accepted
command ID query existing result rather than resubmit new IDs.

## 9. `xenoteerctl`

CLI commands map to SDK operations and diagnostics. Output modes:

- human table/text to stdout;
- stable JSON to stdout;
- progress/logs to stderr.

Exit codes: 0 success, 2 usage/validation, 3 auth, 4 not found/stale, 5 timeout,
6 conflict/resource, 7 server/backend, 8 partial effect. Do not encode raw HTTP
status as process exit code.

Include `doctor` that checks API, capabilities, viewer ticket, extension/profile
status, and optionally runs a harmless pointer/screenshot probe. Destructive
input probes require confirmation flag.

## 10. Configuration structs and validation

Each section is a typed `#[serde(deny_unknown_fields)]` struct with `Default` for
safe compiled defaults. Merge layers as generic TOML/JSON values first, then
deserialize once so unknown fields are caught after precedence.

Use wrappers:

```rust
struct ByteSize(u64);
struct Millis(u64);
struct QueueCapacity(NonZeroUsize);
struct SecretFile(PathBuf); // Debug prints <redacted>
struct AllowedRoot(CanonicalPath);
```

Cross-validation returns all discovered errors at startup, sorted by field path.
Canonicalize allowed roots once; for file operations use directory-relative
safe open patterns where possible to prevent symlink escape/TOCTOU.

## 11. Event hub implementation

Inputs are `NormalizedEvent`. `EventHubTask` assigns sequence and keeps:

- small ring replay buffer by encoded-size and count;
- subscriber map with topic filter and two bounded queues;
- coalescing map keyed only for declared coalescible topics/resources;
- current desktop generation.

Non-coalescible overflow enqueues one `resync_required` in reserved capacity and
closes subscriber. Do not silently overwrite command results, lease changes,
window destroy, or generation events.

## 12. Coding constraints

- `#![forbid(unsafe_code)]` in all first-party crates initially. If SHM/FD needs
  unsafe, isolate in the smallest module/crate, replace forbid with deny only
  there, document invariants, and add Miri/sanitizer tests where applicable.
- No `unwrap`/`expect` in production paths except compile-time/static invariants
  with explanatory message and lint allow at the line.
- No blocking filesystem/process/encoding/X reply waits on Tokio worker threads.
- No locks held across `.await`; Clippy lint/review enforces.
- Public library items have rustdoc and examples where meaningful.
- Exhaustive match on internal state enums; avoid wildcard that hides new state.
- Timeouts use monotonic Tokio/Instant internally; wall clock only for protocol
  deadlines/audit timestamps.
- Random IDs use OS CSPRNG through uuid/rand-supported source.
- Sensitive wrappers implement redacted Debug and are skipped in serialization.
- Every spawned task/thread has a name/owner, cancellation path, and join/error
  observation.

## 13. Test organization

- Unit tests beside pure modules.
- Crate integration tests under each `tests/` directory.
- Shared Xvfb fixture support in a test-only module/crate only if duplication
  justifies it.
- End-to-end tests run against the built container through public SDK/API, never
  internal handles.
- Protocol golden JSON and schema compatibility in `xenoteer-protocol`.
- Property tests for geometry/interpolation/state machines/selectors.
- Fuzz targets for JSON command decode, X property decode, pixel decode, AT-SPI
  cache compatibility, selection INCR, and desktop-entry parsing.

Use one integration-test binary with modules for related expensive container/X
fixtures so Cargo does not unnecessarily serialize many binaries and rebuild
shared setup.

## 14. Implementation order inside Rust

1. Protocol IDs/geometry/errors/envelopes and schema generation.
2. Core interpolation/effect/command-record/lease state machines with properties.
3. X connection probe and XTEST input actor/event recorder fixture.
4. Coordinator ledger + lease + input dispatch.
5. Observation/window model and waits.
6. Capture/pixel pipeline.
7. Clipboard selection actor.
8. AT-SPI cache/query/actions.
9. Process manager.
10. Axum REST/WS, auth, event hub.
11. SDK/CLI.
12. Viewer gateway, artifact policies, telemetry, hardening.

At each step the backend can be exercised through a narrow temporary test
harness, but no alternate public protocol is introduced.

## 15. Code-design exit criteria

- Workspace dependency graph and feature graph match this plan.
- Protocol schema generates deterministically and has compatibility tests.
- Every actor has typed handle/messages, capacity, join, health, and shutdown.
- State-machine property tests cover illegal/terminal transitions.
- Input actor proves same-connection barrier and cleanup through recorder.
- No first-party unsafe or unreviewed exception.
- `cargo fmt --check`, Clippy with warnings denied, tests, docs, cargo-deny, and
  audit pass in CI.
- Container e2e invokes only public API/SDK.

## 16. Primary references

- [x11rb crate and feature model](https://docs.rs/x11rb/latest/x11rb/)
- [x11rb feature list](https://docs.rs/crate/x11rb/latest/features)
- [Rust atspi crate](https://docs.rs/atspi/latest/atspi/)
- [zbus Tokio feature guidance](https://docs.rs/zbus/latest/zbus/)
- [Tokio MPSC sync/async communication](https://docs.rs/tokio/latest/tokio/sync/mpsc/)
- [Tokio process groups and reaping](https://docs.rs/tokio/latest/tokio/process/struct.Command.html)
- [Axum WebSocket split example](https://docs.rs/axum/latest/axum/extract/ws/)
- [Cargo test organization](https://doc.rust-lang.org/cargo/reference/cargo-targets.html)
- [Cargo manifest license expressions](https://doc.rust-lang.org/cargo/reference/manifest.html)
- [Rust Fuzz Book](https://rust-fuzz.github.io/book/)
- [Proptest](https://docs.rs/proptest/latest/proptest/)
- [tracing async diagnostics](https://docs.rs/tracing/latest/tracing/)
