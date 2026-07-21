# Physical input engine

## 1. Goal and boundary

The input engine turns validated high-level commands into globally delivered
X11 device events with deterministic ordering, realistic pointer paths, explicit
state tracking, and bounded cleanup. It uses XTEST through a dedicated x11rb
connection owned by one synchronous actor thread.

It does not use `xdotool` or XSendEvent in its normal path. `xdotool` remains a
diagnostic oracle. Targeted XSendEvent is deliberately excluded because many
applications reject synthetic window-targeted events; global XTEST input follows
normal propagation and passive-grab behavior.

## 2. XTEST constraints that shape the design

The XTEST `FakeInput` request accepts one fake event per request:

- KeyPress/KeyRelease with a physical keycode;
- ButtonPress/ButtonRelease with a physical button;
- MotionNotify with absolute or relative signed 16-bit coordinates;
- a server-side delay in milliseconds.

The server processes requests from one connection in delivery order. XTEST
further specifies that while a fake event's delay is pending, no later requests
from that same client are processed. Therefore:

- one connection serializes Xenoteer input without a mutex shared by async tasks;
- a reply-producing QueryPointer placed after an action is an ordering barrier;
- that connection must not carry window observation, screenshots, clipboard
  service, or unrelated health probes;
- another X client can still act between Xenoteer events, so postconditions must
  report observed rather than assumed state;
- using `XTestGrabControl(impervious=true)` is not a substitute for input
  ownership and is disabled by default.

## 3. Ownership and queueing

### 3.1 Actors and lease

Each desktop has:

- one `ActionCoordinator` in Tokio;
- one bounded MPSC input queue, initial capacity 256;
- one `InputActor` OS thread;
- one dedicated `RustConnection` with XTEST enabled;
- at most one active controller lease.

The coordinator authenticates, validates the desktop generation, checks the
lease, applies command-ID deduplication, checks the deadline, reserves queue
capacity, and sends one typed `InputCommand`. The actor is the only component
allowed to turn an input command into X requests.

Queue admission is FIFO. No priority lane may insert ordinary actions ahead of
accepted actions. Shutdown/reset uses a separate control channel because it must
remain deliverable when the normal queue is full; it still waits for the current
X request/action boundary.

### 3.2 Lease states

```text
Vacant
  -> Held(lease_id, owner, expiry, generation)
  -> Revoking(reason)
  -> Resetting
  -> Vacant
```

- Lease acquisition uses a cryptographically random opaque token and configured
  TTL, initially 60 seconds.
- Valid commands renew activity but not beyond a server maximum without an
  explicit renew call.
- A second acquisition returns `lease_conflict` with redacted holder metadata.
- Lease expiry stops admission, lets an atomic action reach its documented safe
  boundary, performs input reset, then emits `lease.released`.
- WebSocket disconnect does not immediately revoke a lease; a reconnect grace
  period prevents transient network loss from interrupting a drag. Commands
  remain subject to deadline and the lease TTL.
- Human takeover follows the revocation sequence in the product plan; RFB input
  is never enabled merely because a WebSocket disconnected.

## 4. Input state model

The actor owns mutable state:

```text
InputState
  pressed_keys: ordered set<Keycode>
  pressed_buttons: ordered set<PhysicalButton>
  modifier_snapshot: XKB modifier state
  last_observed_pointer: Point
  active_command: Option<CommandId>
  keymap_generation: u64
  connection_generation: u64
  health: Healthy | ResetRequired | Poisoned(reason)
```

State changes are recorded after the X request is successfully serialized to the
connection, not before. Because protocol errors can be asynchronous, every
compound action ends with a reply barrier. An error at the barrier marks the
state `ResetRequired`; the actor attempts conservative releases and establishes
a new observation. If that cannot complete, it becomes `Poisoned` and refuses
new input until the desktop restarts or an operator reset succeeds.

The actor never infers untracked human/application input into its owned pressed
sets. QueryPointer's mask is diagnostic evidence, not permission to release keys
or buttons that Xenoteer did not press.

## 5. Command lifecycle

```text
received -> validated -> queued -> started -> requests_sent
         -> barrier_observed -> completed
                          \-> cleanup -> failed | cancelled
```

The result records timestamps for each transition, requested and observed
pointer state, selected strategy, generated event count, and warnings.

### Deadlines and cancellation

- A command whose deadline has elapsed before start completes as
  `deadline_exceeded` without sending input.
- Once an atomic primitive starts, release one does not promise preemption of
  already submitted XTEST delays. Default interpolated movement is bounded at
  650 ms, making this delay explicit and finite.
- A plain move may cancel between waypoint segments. A key sequence may cancel
  between complete key presses. A multi-notch scroll may cancel between notches.
- Click is atomic from press through release.
- Drag is atomic from press until release; cancellation requests trigger release
  at the next host-controlled segment boundary, not abandonment while held.
- An action can complete after its client deadline if it crossed an atomic
  boundary beforehand. The result is `deadline_exceeded_after_effect` and MUST
  say that side effects occurred; SDKs never retry it automatically.

Do not use Rust `Drop` as the primary cleanup mechanism. Cleanup is an explicit
actor transition because async replies and X requests cannot be guaranteed from
a destructor.

## 6. Pointer movement

### 6.1 Modes

- `instant`: one absolute or relative motion request.
- `linear`: fixed-duration linear interpolation.
- `smooth`: fixed/automatic duration with deterministic smoothstep easing.
- `relative`: resolve the starting pointer immediately before execution, add
  delta with checked arithmetic, then use selected interpolation.
- `waypoints`: ordered points with per-segment or total duration policy.

All modes return `requested_start`, `observed_start`, `requested_end`,
`observed_end`, `duration_ms`, `samples_planned`, and `events_emitted`.

### 6.2 Validation

Before enqueue:

- coordinates and rectangles are finite integers in root physical-pixel space;
- endpoint is within the screen unless `clamp=true` was explicitly requested;
- absolute XTEST coordinates fit signed 16-bit fields;
- duration is 0..10,000 ms; automatic/default remains 80..650 ms;
- sample rate is 1..240 Hz; default 60 Hz;
- waypoint count is 1..1,024 and total encoded command fits the protocol limit;
- easing name is supported; custom executable expressions are forbidden.

Clamping is never implicit. When enabled, the result reports original and
clamped coordinates.

### 6.3 Automatic duration

For Euclidean distance `d` pixels and nominal speed `v=1200 px/s`:

```text
if d == 0: duration = 0
otherwise: duration_ms = clamp(round(1000*d/v), 80, 650)
```

The nominal speed and clamps are configuration defaults and become stable only
after the conformance matrix. Explicit caller duration bypasses the automatic
formula but remains inside safety limits.

### 6.4 Deterministic sampling and exact time

Given start `p0`, destination `p1`, duration `D` integer ms, and rate `H` Hz:

```text
N = max(1, ceil(D * H / 1000))
t_i = i / N, for i in 1..N
e_i = t_i                       for linear
e_i = 3*t_i^2 - 2*t_i^3        for smooth
point_i = round_ties_away(p0 + e_i * (p1 - p0))
cumulative_i = round(i * D / N)
delay_i = cumulative_i - cumulative_(i-1)
```

Implementation requirements:

1. Calculate with `f64`; convert only after range checking.
2. Define `round_ties_away` in the core crate and property-test it; do not rely
   on language-specific client rounding.
3. Accumulate the delay of duplicate rounded points into the next emitted point.
4. Preserve the final sample even if it duplicates the preceding rounded point,
   so the exact endpoint is the last event and total server delay remains `D`.
5. Sum of emitted delays MUST equal exactly `D`.
6. The sequence must be monotonic per axis when start/end define a monotonic
   segment; easing must not overshoot.
7. After sending all requests, issue QueryPointer on the same connection and
   await its reply.

If `D=0`, emit one event with zero delay. If start equals destination, default
behavior performs no motion request and returns the observed point; callers may
request `emit_noop=true` for hover-refresh diagnostics.

### 6.5 Waypoints

Waypoints are normalized to include the current observed start and requested
final endpoint. Remove consecutive identical caller points before duration
allocation, while preserving an explicitly requested final no-op only under the
diagnostic flag.

Duration policies:

- `per_segment`: caller gives each segment duration.
- `total`: allocate total duration proportional to segment length using the same
  cumulative rounding technique so allocations sum exactly.
- `automatic`: calculate each segment from nominal speed and clamp the entire
  path to the configured maximum only if explicitly allowed; otherwise each
  segment retains its clamp.

For plain waypoint moves, cancellation is checked between segments. For drags,
the actor checks cancellation between segments and immediately schedules a
button release before returning.

### 6.6 Pointer interference

Applications may warp the pointer and another authorized X client could move it.
Xenoteer does not grab the server or continuously correct the path. Endpoint
policy:

- `observe` (default): return success with a warning if X accepted requests but
  observed endpoint differs; high-level click/drag treats mismatch as failure
  before pressing/releasing according to its stage.
- `require`: return `postcondition_failed` on mismatch.
- `correct_once`: send one instant correction, barrier, and report both
  observations; never loop indefinitely.

## 7. Clicks and buttons

### 7.1 Button mapping

Query server pointer mapping at actor startup and on mapping notification.
Public logical buttons map initially:

```text
left=1, middle=2, right=3,
scroll_up=4, scroll_down=5,
scroll_left=6, scroll_right=7,
back=8, forward=9
```

XTEST interprets its detail as a physical button and applies current mapping.
If the requested logical button has no unambiguous physical mapping, return
`unsupported_by_backend`; do not guess.

### 7.2 Click algorithm

An optional-target click executes atomically:

1. move using requested path;
2. require exact endpoint unless caller explicitly selects observe-only;
3. apply pre-click dwell, default 30 ms, on the ButtonPress request delay;
4. send press and add button to owned pressed set;
5. send release with press-duration delay, default 50 ms;
6. remove owned button;
7. issue QueryPointer barrier and verify endpoint/mask;
8. apply post-click settle as a wait/postcondition policy, not an unexplained
   sleep hidden inside the low-level action.

If press serialization succeeds and any later step fails, cleanup attempts a
zero-delay release before returning. The result identifies `effect_stage` as
`none`, `moved`, `pressed`, or `released`.

### 7.3 Multiple/double click

- `count` is 1..5.
- Pointer movement occurs once before the sequence.
- Each press/release pair is complete and barrier-checked at sequence end.
- Inter-click interval defaults to 100 ms and must remain below the desktop's
  fixed double-click threshold, initially 250 ms.
- All clicks use the exact same coordinate unless an explicit path is supplied.
- Cancellation is accepted only between complete clicks; the result reports
  completed count so clients do not replay ambiguously.

Raw `button_down` and `button_up` are advanced operations. They require the
controller lease, reject duplicate down/up by default, and are forcibly reset on
lease loss. `allow_redundant=true` is diagnostic only and does not modify owned
state twice.

## 8. Dragging

Drag request:

```text
start? -> endpoint -> button -> movement policy
press_dwell -> path/waypoints -> release_dwell -> endpoint policy
```

Algorithm:

1. query pointer; move to explicit start if present;
2. barrier and require start endpoint;
3. send ButtonPress and mark owned;
4. optional press dwell, initial 50 ms;
5. traverse path; maintain button without extra events;
6. if cancellation/error occurs at a segment boundary, skip remaining points;
7. send ButtonRelease regardless of traversal outcome;
8. clear owned state, barrier, and observe pointer/button mask;
9. return traversal and cleanup outcomes independently.

If the application establishes a pointer grab, XTEST events still follow normal
grab semantics. A successful X request does not prove the intended widget
handled the drag. High-level APIs should wait for a caller-specified visual,
window, or semantic postcondition.

Do not implement drag as three separately queued public commands. Atomicity is
the feature.

## 9. Scrolling

Release one implements discrete core-button scrolling:

- vertical notches use logical buttons 4/5;
- horizontal notches use 6/7 only when mapping/capability probe succeeds;
- each notch is press+release;
- `count` is bounded initially to 1..1,000;
- interval defaults to 16 ms and may be 0..1,000 ms;
- cancellation is checked between notches;
- result reports completed notches and direction.

High-resolution/smooth XI2 valuator scrolling is deferred. "Smooth scrolling"
must not be used to describe paced button notches in API names or docs.

## 10. Reset and poisoning

`reset_owned_input` executes in reverse press order:

1. zero-delay releases for owned buttons;
2. zero-delay releases for owned non-modifier keys;
3. zero-delay releases for owned modifier keys;
4. same-connection QueryPointer barrier;
5. clear state only for releases that reached a successful barrier;
6. emit a reset report with residual observed mask and errors.

If the X connection is broken, reconnecting cannot prove what the old connection
caused. The actor reconnects only for observation/reset attempts, marks itself
poisoned, and requires a desktop restart before ordinary input. This is safer
than claiming a clean state after an unknowable disconnect.

## 11. Diagnostics and metrics

Every action span includes `desktop_generation`, hashed/opaque `command_id`,
lease ID hash, kind, queue delay, execution time, event count, requested/observed
endpoint, and effect stage. It excludes typed text, clipboard content, auth
tokens, and screenshots.

Metrics:

- queue depth and admission rejections;
- action latency histogram by low-cardinality action kind/outcome;
- interpolation sample/event counts;
- endpoint mismatches;
- cleanup attempts/failures;
- lease conflicts/expiry;
- actor reconnect/poison events.

Never use command, window title, application name, or element selector as a
Prometheus label.

## 12. Required tests

### Pure/property tests

- duration and delay sums are exact for all allowed durations/rates;
- final sample equals destination;
- no overshoot and axis monotonicity;
- duplicate compaction preserves total time;
- proportional waypoint allocation preserves total;
- coordinate/range/overflow validation;
- state transition legality and reverse-order cleanup.

### X11 integration tests

Use an event-recorder fixture that maps a window and records motion, key,
button, focus, timestamps, coordinates, and state masks:

- instant/linear/smooth/relative/waypoint paths;
- zero-distance, one-pixel, diagonal, edge, corner, max-duration paths;
- click/double-click and all mapped buttons;
- drag success, cancel, target destruction, and application pointer grab;
- vertical and horizontal scrolling;
- connection error and delayed request behavior;
- QueryPointer barrier endpoint;
- concurrent clients cannot interleave through the coordinator;
- xdotool comparison for a representative action set.

### Failure injection

- deadline before queue/start and during atomic action;
- queue full;
- WebSocket disconnect;
- lease expiry;
- actor panic converted to critical service failure;
- Xvfb killed during press/drag;
- application warps pointer;
- invalid/remapped button map;
- shutdown during each effect stage.

No test passes merely because the API returned 2xx; the recorder or
postcondition must prove delivered behavior.

## 13. Primary references

- [XTEST extension protocol](https://www.x.org/releases/X11R7.6/doc/xextproto/xtest.html)
- [X11 client request ordering](https://www.x.org/guide/communication/)
- [X11 core protocol and QueryPointer](https://www.x.org/releases/X11R7.6/doc/xproto/x11protocol.pdf)
- [x11rb protocol extensions](https://docs.rs/x11rb/latest/x11rb/protocol/index.html)
- [xdotool manual and XSendEvent caveat](https://manpages.debian.org/testing/xdotool/xdotool.1.en.html)
- [xdotool upstream](https://github.com/jordansissel/xdotool)
