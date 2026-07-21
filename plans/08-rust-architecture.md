# Rust architecture: critical design pass

## 1. Architectural objective

Build `xenoteerd` as a small control-plane kernel around fallible external
systems. X11, D-Bus/AT-SPI, child processes, WebSockets, and the viewer are not
reliable in-process libraries with transactional semantics. The architecture
must preserve Xenoteer's invariants even when those systems reorder events,
disappear, time out, return malformed data, or partially perform a command.

The design favors explicit ownership and message passing over shared connection
locks. It keeps public protocol types independent from backends and makes every
side effect attributable to a command, lease, desktop generation, and trace.

## 2. Critical-thinking pass

This section deliberately challenges the happy-path architecture before naming
components.

### 2.1 Assumptions we are willing to make

- One container owns one Xvfb/XFCE desktop in release one.
- Xenoteer is the only authorized bot input source.
- RFB is server-side view-only, so human input cannot race the input actor.
- All desktop applications run in the same X11 security domain and can in
  principle observe/inject X events; the container boundary, not X11, is the
  primary tenant boundary.
- X11 and D-Bus are local Unix transports, so normal round trips are cheap but
  still fallible.
- An atomic input primitive may be temporarily non-preemptible for a bounded
  interval (default path <=650 ms).
- A desktop restart invalidates all references and in-flight operations.

If any assumption changes—especially multi-tenant apps or collaborative human
input—the control and security models need redesign, not a flag.

### 2.2 Assumptions we reject

- "The request succeeded" does not mean the application handled it.
- A cached pointer position, window rectangle, or element extent is not current
  by the time an input command executes.
- XID/object paths/PIDs/titles are not durable identities alone.
- Cancellation does not undo input already delivered.
- Dropping a future does not kill a child process or release held input.
- WebSocket reconnect does not make a non-idempotent command safe to replay.
- AT-SPI trees are not complete or consistent, and screenshots are not semantic.
- `view_only` in browser JavaScript is not a server security control.
- Root in a container is not harmless, and `--no-sandbox` is not an acceptable
  browser compatibility fix.

### 2.3 Invariants that dominate convenience

1. Only `InputActor` mutates physical input state.
2. Every externally visible reference is desktop-generation-bound.
3. A command gets at most one accepted execution per dedupe window.
4. Partial effects are reported; errors do not erase history.
5. Side-effect cleanup is explicit and bounded.
6. Bounded queues isolate slow consumers from protocol event loops.
7. No network task performs blocking X11/encoding/process cleanup work directly.
8. Backend types do not leak into the public schema.
9. Security capabilities are checked before resource resolution and again before
   effect when queued state can change.
10. Readiness is a state machine derived from capability probes.

### 2.4 Stress scenarios and architectural answers

| Scenario | Wrong local fix | Architectural answer |
|---|---|---|
| Smooth mouse move blocks screenshot | Put connection behind mutex and wait | Dedicated input and capture connections/actors |
| Client retries after lost response | Execute JSON again | Command-ID result ledger; ambiguous effects never auto-replayed |
| Disconnect during drag | Rely on future cancellation/drop | Actor-owned pressed state and explicit release transition |
| Window destroyed/reused in queue | Use stored XID | Generation/identity validation near execution |
| AT-SPI floods browser events | Unbounded broadcast channel | Bounded normalization hub, coalescing, resync marker |
| VNC user moves mouse | Update bot's cached position | Forbid RFB input; later takeover revokes lease transactionally |
| X connection breaks after key press | Reconnect and clear Rust set | Mark input poisoned; reset best effort; require clean desktop restart |
| App ignores close/focus | Return success after ClientMessage | Observe EWMH/process postcondition and return refusal/timeout |
| API task times out child launch | Drop `Child` handle | Managed process actor owns group, signals, reaping, and output |
| Huge a11y tree/screenshot | Allocate according to peer length | Preflight and aggregate budgets at every boundary |
| Viewer backend becomes abandoned/vulnerable | Rewrite public API around it | Adapter boundary plus conformance suite |

### 2.5 Alternatives considered and rejected for release one

- **Shelling out to xdotool for control:** simple, but incurs subprocess races,
  parsing, weak state ownership, and XSendEvent pitfalls. Keep only as oracle.
- **One async X11 connection:** easy initially, but delayed XTEST and large image
  replies create head-of-line blocking and mutable ownership ambiguity.
- **One global `Mutex<AppState>`:** invites locks across `await`, lock ordering,
  and unrelated state coupling. Use actors/snapshots and small atomic metadata.
- **AT-SPI-only automation:** excludes inaccessible/custom applications and does
  not provide realistic input semantics.
- **Computer vision first:** costly and probabilistic; does not replace protocol
  state. Leave a future observation adapter point.
- **gRPC/protobuf initially:** strong schema but heavier browser/SDK integration.
  Versioned JSON schemas and generated types are enough; wire framing can evolve.
- **systemd inside container:** adds cgroup/host assumptions and privileges not
  needed for a one-desktop service graph. s6-rc fits the runtime.
- **Native framebuffer streamer first:** large codec/WebRTC scope. Use replaceable
  RFB stack to reach a usable viewer, then measure.
- **Restart Xvfb in place:** existing applications cannot survive connection loss
  coherently. Terminate/recreate the container/desktop generation.

## 3. System components

```text
                     +-----------------------+
HTTP/WS connections -> API session tasks     |
                     +-----------+-----------+
                                 |
                     +-----------v-----------+
                     | Auth + ActionCoordinator
                     | lease / dedupe / deadlines
                     +--+--------+--------+--+
                        |        |        |
          +-------------v+  +----v-----+  +v--------------+
          | InputActor   |  | Process  |  | Semantic API  |
          | OS thread/X  |  | Manager  |  | AtspiActor    |
          +--------------+  +----------+  +---------------+
                        |        |        |
          +-------------v--------v--------v---------------+
          | ObservationActor | ClipboardActor | CaptureActor
          +------------------+----------------+------------+
                                 |
                     +-----------v-----------+
                     | EventHub + snapshots  |
                     +-----------+-----------+
                                 |
                     +-----------v-----------+
                     | DesktopSupervisor     |
                     | readiness/generations |
                     +-----------------------+
```

The dependency arrows in code point inward toward protocol/domain crates.
Backend adapters implement domain ports. API handlers do not call x11rb/zbus
directly.

## 4. Process model

`xenoteerd` is one non-root process with:

- one multi-threaded Tokio runtime for network, timers, zbus, coordination, and
  bounded encoding dispatch;
- dedicated standard threads for x11rb actors whose blocking receive/event/reply
  loops need strict connection ownership;
- a bounded blocking pool/semaphore for PNG and expensive snapshot serialization;
- supervised child application groups, but not Xvfb/XFCE/viewer services (s6
  owns those service processes).

Why synchronous X actor threads: x11rb's pure Rust connection and X11 request/
event model are naturally sequential, and XTEST server delays are intentional.
Putting the connection on an OS thread makes ownership and blocking explicit.
Tokio communicates through bounded `mpsc`; synchronous actors use
`blocking_recv` and reply through oneshot senders, which can be used outside the
runtime.

## 5. Layering and dependency rule

### Protocol layer

Serializable request/response/event types, version identifiers, JSON schema,
validation-independent shape. No x11rb, zbus, Axum, Tokio runtime handles, file
descriptors, or backend error types.

### Domain/core layer

IDs, references, coordinate types, actions, policies, normalized snapshots,
error taxonomy, capability set, state machines, and backend traits/ports. Pure
algorithms such as interpolation and selector predicates live here.

### Adapters

- X11 input/window/capture/clipboard;
- AT-SPI;
- process manager;
- artifact store;
- viewer supervisor/status;
- authentication provider;
- metrics/tracing exporters.

Adapters convert external errors into structured internal errors with a source
chain for logs, then into safe protocol problems.

### Application/composition

`xenoteerd` loads configuration, creates actors, wires ports into coordinators,
starts API/listeners, and drives graceful shutdown. It contains no reusable
XTEST math or selector logic.

## 6. State ownership matrix

| State | Sole writer | Readers receive |
|---|---|---|
| pressed keys/buttons, input health | InputActor | action result / watch snapshot |
| controller lease | ActionCoordinator | immutable lease snapshot/events |
| dedupe result ledger | ActionCoordinator | command result lookup |
| window model/X event sequence | ObservationActor | revisioned snapshot/events |
| clipboard ownership/transfers | ClipboardActor | selection metadata/results |
| pixel capture buffers | CaptureActor | owned encoded/raw result |
| accessibility cache | AtspiActor | revisioned normalized snapshot/events |
| managed processes | ProcessManager | process references/events |
| desktop readiness/generation | DesktopSupervisor | Tokio watch channel |
| per-WS outbound queue | connection writer task | no other writer |
| configuration | immutable `Arc<Config>` | read-only typed sections |

No module gets a mutable reference to another module's state. Cross-subsystem
transactions are sagas coordinated by `ActionCoordinator`, with compensating
cleanup and an effect journal—not ACID transactions.

## 7. Action coordination

### 7.1 Admission order

For every command:

1. decode under body/message limit;
2. authenticate principal;
3. authorize capability and desktop scope;
4. validate protocol version/schema/values;
5. check desktop generation/readiness;
6. look up command ID in result ledger;
7. verify lease for mutating physical input;
8. reserve queue capacity;
9. record `accepted` result-ledger entry and trace span;
10. dispatch typed operation;
11. record terminal/partial result once;
12. send response/event.

The accepted ledger entry closes the race between duplicate requests arriving
concurrently. A duplicate while original runs receives `in_progress` plus a
result subscription/reference; it does not create a second future.

### 7.2 Dedupe ledger

Bounded LRU+TTL, initially 10,000 results or 15 minutes. Entry stores principal
scope, desktop generation, canonical request hash, lifecycle, terminal result or
safe summary, and timestamps.

Reusing a command ID with a different canonical request hash returns
`command_id_conflict`. IDs are scoped to authenticated principal and desktop.
Large binary results/artifacts are referenced, not retained inline. Persistence
across daemon/container restart is not promised in release one; restart changes
desktop generation, so clients must not retry old mutations.

### 7.3 Effect journal

Every mutating operation reports monotonically advancing effect stage, for
example:

```text
none -> pointer_moved -> button_pressed -> button_released -> postcondition_met
```

Errors carry the last stage and cleanup result. This is the basis for safe SDK
behavior and forensic traces.

## 8. Desktop lifecycle and generations

`DesktopSupervisor` consumes s6/service probes and actor health:

```text
Booting -> Probing -> Ready
                    -> Degraded
Ready/Degraded -> Draining -> Stopped
any pre-stop -> Failed
```

Generation is created once for an Xvfb/session lifetime. It is random plus
monotonic process-local metadata so references cannot accidentally match after
restart. Actor-local reconnections have their own generations when the desktop
survives (for example AT-SPI bus reconnect).

Readiness transitions are serialized and emitted. API requests capture the
generation at admission and adapters recheck it near effects. A transition out
of Ready cancels queued generation-bound work before start.

## 9. Concurrency and backpressure

Rules:

- Every channel has a documented capacity and overflow behavior.
- Awaiting `send` is allowed only where upstream backpressure is safe (API
  admission). Protocol event loops use try/coalesce/resync, not indefinite await.
- Never hold `std::sync::Mutex` or a tracing span enter guard across `.await`.
- CPU-heavy encoding/large JSON serialization is behind semaphores and
  `spawn_blocking`/dedicated workers with admission limits.
- One WebSocket writer owns the sink; producers send typed outbound messages.
- Tasks are children of a subsystem cancellation token/task tracker and are
  joined during shutdown.
- Detached `tokio::spawn` without ownership/join/error reporting is forbidden in
  production paths.

Fairness: physical input remains FIFO. Read-only API calls use independent
subsystem queues. One client's expensive semantic snapshot cannot consume all
query permits; global plus per-principal semaphores apply.

## 10. Cancellation and shutdown

Cancellation is cooperative and operation-specific. A cancellation token does
not imply rollback. Domain operations declare safe boundaries and cleanup
actions. Coordinator returns:

- `cancelled_before_effect`;
- `cancelled_after_effect` with journal;
- `deadline_exceeded_before_effect`;
- `deadline_exceeded_after_effect`;
- terminal success/failure.

Shutdown sequence follows [01](01-container-runtime.md). In process:

1. global token enters draining;
2. listeners stop/admission closes;
3. leases revoke/queues close and drain according to policy;
4. input reset completes or reaches timeout;
5. child groups terminate/reap;
6. WS writers close;
7. X/D-Bus actors stop and join;
8. telemetry flushes within bound;
9. main returns meaningful exit code for s6.

Panics in critical actor threads are caught at the join/supervision boundary,
mark capability failed, trigger shutdown, and never cause silent actor loss.
`panic=abort` is not selected initially because graceful input/process cleanup is
valuable; production may revisit after fault tests.

## 11. Error architecture

Internal error:

```text
XenoteerError {
  code: ErrorCode,
  operation,
  safe_message,
  retry: Never | SameCommandId | AfterResync | AfterBackoff,
  effect_stage,
  desktop_generation,
  resource_refs,
  details: bounded safe map,
  source: private anyhow/error chain
}
```

Categories:

- validation/protocol version;
- authentication/authorization;
- lease/conflict/deduplication;
- stale/not found/ambiguous;
- unsupported/capability degraded;
- timeout/cancel/backpressure/resource exhausted;
- target refused/vanished/postcondition failed;
- X11/D-Bus/process/viewer/artifact backend;
- poisoned/desktop generation changed/internal invariant.

Protocol errors use stable code and RFC 9457-shaped fields for HTTP. Safe
messages never include paths outside allowed roots, environment secrets,
clipboard/text content, raw D-Bus messages, or panic strings.

## 12. Configuration architecture

One `Config` tree is loaded once, validated as a whole, and immutable. Sections:

- server/listeners/TLS proxy trust;
- auth/principals/capabilities;
- desktop/display/environment/profile;
- input/keyboard/clipboard;
- windows/accessibility/capture/viewer;
- process launch/workspace;
- queue/budget/timeout limits;
- artifacts/logging/metrics;
- shutdown and compatibility flags.

Configuration parser denies unknown fields. Durations/byte sizes use explicit
human-readable deserializers but diagnostics render canonical units. Secrets are
`SecretString`-like wrappers excluded from Debug/Serialize and normally loaded
from files.

Startup validates cross-field conflicts: view-only required, ports distinct,
limits internally consistent, read-only paths writable, display fits XTEST,
automatic timeout >= maximum primitive duration, and required capabilities
match image profile.

## 13. Security architecture

Treat the API as remote code/control over a desktop. Authorization is capability
based:

- `desktop:observe`
- `input:control`
- `clipboard:read`, `clipboard:write`
- `accessibility:read`, `accessibility:write`
- `application:launch`, `application:terminate`
- `artifact:read`, `viewer:read`
- advanced raw keycode/executable/path capabilities separately.

Authentication middleware produces a principal before desktop/resource lookup
to reduce enumeration. Object-level scope is checked on every reference. Rate,
concurrency, byte, and duration quotas defend resource consumption. Detailed
policy is in [12](12-security-and-isolation.md).

## 14. Observability

Use `tracing` spans because async tasks interleave and a log line alone loses
causality. Root fields: request ID, command ID hash, principal ID hash, desktop
generation, operation, queue/execution times, effect stage, error code. Do not
hold `Span::enter` guards across await; instrument futures/spans correctly.

Metrics have low-cardinality labels only. Optional OpenTelemetry export uses
semantic conventions where applicable, but local structured JSON logs and
Prometheus endpoint are sufficient for release one.

The daemon exposes a bounded diagnostic state dump without secret values or
large content. Support bundles are explicit artifact operations with redaction.

## 15. Evolution seams

Ports/adapters permit, without changing core command semantics:

- alternative RFB/native viewer;
- Wayland input/window backend in a future major capability set;
- OCR/image locator producing explicit visual references;
- external artifact store;
- persistent command ledger;
- multiple desktops per process (not merely enabled by looping; actor/scope tests
  would be required);
- alternate auth provider.

Do not introduce an abstraction until there are either two implementations or a
known isolation/testing need. Backend traits should express Xenoteer domain
operations, not mirror every upstream call.

## 16. Architecture acceptance checklist

- Dependency graph has no adapter -> API or protocol -> backend cycles.
- Every mutable state item has one named writer.
- Every channel/queue/semaphore has a capacity and overflow behavior.
- Every operation declares idempotency, effect stages, timeout, and cancellation
  boundaries.
- Every external reference carries generation and stale behavior.
- Every child task/thread is supervised and joined.
- No normal handler calls shell, xdotool, x11vnc control, or raw X connection.
- Fault tests prove actor death and connection loss transition readiness.
- Unsafe Rust is absent initially or isolated/documented/audited if a dependency
  interface ultimately requires it.
- Public schema can compile without X11/D-Bus system libraries.

## 17. Primary references

- [Rust shared-state concurrency](https://doc.rust-lang.org/stable/book/ch16-03-shared-state.html)
- [Tokio MPSC sync/async guidance and clean shutdown](https://docs.rs/tokio/latest/tokio/sync/mpsc/)
- [Tokio process ownership caveats](https://docs.rs/tokio/latest/tokio/process/struct.Command.html)
- [zbus Tokio integration](https://docs.rs/zbus/latest/zbus/)
- [Axum WebSocket concurrency](https://docs.rs/axum/latest/axum/extract/ws/)
- [tracing structured spans/events](https://docs.rs/tracing/latest/tracing/)
- [RFC 9457 Problem Details](https://datatracker.ietf.org/doc/html/rfc9457)
- [X11 request ordering](https://www.x.org/guide/communication/)
- [XTEST delayed request semantics](https://www.x.org/releases/X11R7.6/doc/xextproto/xtest.html)
