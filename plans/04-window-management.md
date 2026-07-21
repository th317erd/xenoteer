# X11 window management and references

## 1. Goal

Expose stable-enough, generation-bound window discovery and control over a
cooperating EWMH window manager while acknowledging X11's asynchronous and
advisory nature. Operations request changes through the window manager, observe
the resulting properties/events, and return requested versus actual state.

Xenoteer is not a replacement window manager and does not seize control from
XFCE. Raw core requests are fallbacks with explicit warnings, not silent ways to
bypass EWMH policy.

## 2. Sources of truth

The observation actor maintains a model from:

- root `_NET_CLIENT_LIST` and `_NET_CLIENT_LIST_STACKING`;
- `CreateNotify`, `MapNotify`, `UnmapNotify`, `DestroyNotify`,
  `ReparentNotify`, `ConfigureNotify`, `PropertyNotify`, and focus events;
- EWMH properties on root and top-level clients;
- ICCCM properties and `WM_PROTOCOLS`;
- process evidence such as `_NET_WM_PID` plus `/proc/<pid>/stat` start time when
  the PID is in the same namespace;
- X geometry/tree queries used to reconcile event loss or initial state.

Events are hints to update the cache, not infallible truth. On startup,
reconnection, event sequence gap, or suspicious property error, rebuild a
snapshot from root properties and the window tree.

## 3. Window identity

### 3.1 Public reference

```text
WindowRef {
  desktop_id,
  desktop_generation,
  xid,
  observed_generation,
  identity_hash
}
```

`identity_hash` is server-generated from non-secret initial evidence: XID,
first-observed model sequence, process identity if available, WM_CLASS, and
client leader. It prevents an old serialized reference from silently following
an XID that has been destroyed and reused.

`observed_generation` increments when a previously absent XID appears. Cache
tombstones retain destroyed `(xid, observed_generation)` values for at least the
command deduplication TTL.

### 3.2 Validation

Every reference-taking command:

1. checks desktop ID/generation;
2. looks up XID and observed generation;
3. verifies identity hash;
4. optionally performs a lightweight attribute/property query before a
   destructive or input-targeting action;
5. returns `stale_reference` if evidence no longer matches.

Never auto-search by title/class after a stale reference. Callers can explicitly
run the selector again.

### 3.3 Process identity caveat

`_NET_WM_PID` is optional, client-supplied, and not authentication. Pair it with
PID namespace context and `/proc` start time when possible. It aids correlation
but never authorizes operations. D-Bus-activated apps and multi-process browsers
may have window PIDs different from the launcher PID.

## 4. Window snapshot schema

Each snapshot includes nullable/diagnostic fields rather than inventing values:

- `ref`, `xid_hex`, model sequence;
- title from `_NET_WM_NAME` UTF-8, fallback `WM_NAME` decoded per type;
- visible title/icon title where supplied;
- `WM_CLASS` instance/class as two separate strings;
- PID/process reference, client machine, client leader;
- window type(s), state set, allowed actions;
- mapped/viewable/minimized/hidden/urgent/modal flags;
- root-frame rectangle, client rectangle, optional frame extents;
- current workspace/desktop and sticky state;
- transient-for/group relationships;
- active/focused status;
- stacking index;
- supported protocols (`WM_DELETE_WINDOW`, `WM_TAKE_FOCUS`);
- accessibility application correlation, if established;
- warnings for malformed/truncated properties.

Strings are length-bounded and decoded lossily only when necessary; the result
flags lossy decoding. Property reads use a maximum byte budget and handle
multi-part retrieval without unbounded allocation.

## 5. Selector model

Selectors are data, not regular expression code supplied to a shell. Supported
predicates:

- exact/contains/prefix/suffix/regex title, with bounded regex complexity;
- exact WM_CLASS instance or class;
- process reference/PID;
- window type/state/mapped/visibility/workspace;
- active/focused;
- transient/group parent;
- has accessibility application;
- creation sequence after a supplied observation point.

Composition supports `all`, `any`, and `not`, with maximum depth and predicate
count. Results have explicit ordering (`creation`, `stacking`, `title`, `xid`)
and limit. A single-target operation fails with `ambiguous_target` unless the
selector produces exactly one or the caller explicitly chooses `first` under a
declared order.

Regex uses a linear-time engine where possible and has input/compiled-size
limits. Case folding is locale-independent Unicode; raw-byte matching is not a
public feature.

## 6. Coordinate and geometry model

Distinguish:

- `client_rect`: top-level client window geometry translated to root;
- `frame_rect`: window-manager frame geometry;
- `content_rect`: client area after optional toolkit/frame information; initially
  equal to client rect unless proven;
- `_NET_FRAME_EXTENTS`: advisory borders supplied by WM;
- AT-SPI element extents: separate semantic geometry.

Window-relative physical clicks default to the client origin, not the frame
origin. APIs require a `relative_to` enum when ambiguity matters.

Moving/resizing requests specify desired outer frame or client geometry
explicitly. The result returns both desired and observed forms because window
manager constraints, size hints, decorations, and screen edges may alter them.

## 7. EWMH operations

### 7.1 Capability discovery

At desktop readiness, read `_NET_SUPPORTED` and publish individual capabilities.
An operation whose required atom is absent returns `unsupported_by_window_manager`
unless a documented fallback was explicitly requested.

### 7.2 Activate/focus

Preferred sequence:

1. validate mapped target and switch it to the active workspace if requested;
2. send `_NET_ACTIVE_WINDOW` ClientMessage to root with source indication 2
   (pager/automation controller), a meaningful last-user timestamp when known,
   and current active window;
3. wait for root `_NET_ACTIVE_WINDOW` and relevant FocusIn evidence;
4. return requested/observed active and focused windows.

Raw `SetInputFocus` is an opt-in fallback only when EWMH activation is unsupported
or a tested legacy application needs it. It can violate WM policy and does not
raise/activate the frame reliably. Physical input methods require successful
activation by default; `allow_unfocused=true` is advanced and traced.

### 7.3 Close

Preferred close sends `_NET_CLOSE_WINDOW` to root. If unsupported, send
`WM_DELETE_WINDOW` only when listed in `WM_PROTOCOLS`. Never translate close to
SIGKILL automatically.

Close wait outcomes:

- `destroyed`: XID disappeared;
- `unmapped`: still exists but no longer mapped;
- `refused_or_timed_out`: application did not close;
- `process_exited`: correlated managed process exited;
- `replaced`: original destroyed and an expected successor appeared.

Force-killing a managed application process group is a separate privileged API
operation with its own authorization and grace sequence.

### 7.4 Minimize, maximize, fullscreen, above, sticky

Use `_NET_WM_STATE` add/remove/toggle requests with the relevant atoms. Public
operations use desired boolean state rather than toggle by default, because
toggle is not idempotent under retry. Observe property convergence and report
WM refusal/partial results.

Minimization is requested through the WM-supported mechanism and observed via
mapped/hidden/iconic state. EWMH warns that `_NET_WM_STATE_HIDDEN` is WM-managed;
clients do not set it directly.

### 7.5 Move and resize

Use `_NET_MOVERESIZE_WINDOW` for programmatic geometry when supported. Preserve
unspecified axes/sizes with the request's validity flags. Respect:

- ICCCM `WM_NORMAL_HINTS` min/max/base size, resize increments, and aspect;
- screen bounds policy;
- decorations/frame extents;
- WM placement policy.

Wait for a quiet geometry window (initially 50 ms with no ConfigureNotify, max
1 second) rather than assuming the first configure is final. Return actual
geometry and constraint warnings. Interactive `_NET_WM_MOVERESIZE` is not used
for ordinary API resizing because it models a human pointer-driven operation.

### 7.6 Raise/lower and stacking

Prefer WM state/activation. Raw ConfigureWindow stack-mode is best effort and may
be overridden. Do not promise an exact global z-order; expose observed stacking
list and whether requested relative order converged.

### 7.7 Workspaces

Release-one XFCE profile has one workspace. Schema and observation still carry
workspace IDs so a future fixed multi-workspace profile is compatible. Methods
that create/delete workspaces are deferred. Switching/moving windows may be
implemented only after tests cover `_NET_CURRENT_DESKTOP`, `_NET_WM_DESKTOP`,
sticky windows, and focus interactions.

## 8. Waits and events

Every model mutation increments a desktop-scoped `event_sequence`. Window events
include a complete `WindowRef`, changed fields, before/after where bounded, and
the sequence.

Wait primitives:

- `window.wait_for(selector, state, timeout)`;
- `window.wait_until_closed(ref)`;
- `window.wait_for_geometry(ref, predicate)`;
- `window.wait_for_active(ref)`;
- `window.wait_for_count(selector, comparison)`.

Race-free wait registration:

1. capture model sequence;
2. evaluate predicate against snapshot;
3. subscribe starting after captured sequence;
4. re-evaluate if a gap/rebuild occurred;
5. complete on predicate or timeout.

This prevents the classic check-then-subscribe missed event. On event backlog
overflow, send a `resync_required` marker; waits re-read the snapshot.

## 9. Interaction with input and capture

High-level physical action targeting a window:

1. validate `WindowRef`;
2. optionally activate and await focus;
3. resolve current geometry immediately before enqueueing input;
4. translate/clamp target coordinates under explicit policy;
5. pass desktop-root coordinates and identity precondition to input actor;
6. input actor asks observation coordinator to revalidate if queue delay exceeds
   the staleness threshold, initially 100 ms;
7. send physical input;
8. caller optionally waits on a window/semantic/visual postcondition.

Window capture through core GetImage is only reliable for viewable, unobscured
content unless backing store/Composite is used. The public API names root versus
window capture precisely and returns capture limitations; see [06](06-observation-and-streaming.md).

## 10. Error model

- `window_not_found`
- `ambiguous_target`
- `stale_reference`
- `window_not_viewable`
- `unsupported_by_window_manager`
- `window_manager_refused`
- `target_vanished`
- `focus_not_acquired`
- `geometry_constrained`
- `postcondition_failed`
- `timeout`
- `malformed_window_property`
- `x11_protocol_error`

Constraint/refusal results can contain a useful final snapshot. Errors do not
discard observed state.

## 11. Gotchas and tests

| Gotcha | Required test/decision |
|---|---|
| XID reused after destroy | Tombstone and identity-hash stale-reference test |
| Title changes frequently | Property event and selector wait test; no identity by title |
| Browser launcher PID differs from window PID | Process-group/client-leader/AT-SPI correlation fixture |
| WM ignores activation | Return focus failure; never send target input by default |
| `SetInputFocus` appears to work but frame inactive | Fallback is opt-in and both active/focused states reported |
| Resize constrained by hints | Fixture with increments/aspect/min/max; actual geometry returned |
| Decorations confuse coordinates | Client/frame/extents fixture and click-relative-to-client test |
| Window closes then replacement dialog appears | Closed original ref remains stale; selector finds successor explicitly |
| Event lost or queue overflow | Snapshot reconciliation and `resync_required` test |
| Minimized window capture is empty/invalid | Explicit capture error/Composite path, never stale pixels labeled current |
| Malformed properties allocate huge buffers | Property byte caps and fuzzed parser |
| Toggle duplicated after retry | SDK uses desired boolean; command-ID dedupe test |

Toolkit fixture matrix includes GTK, Qt, Chromium multi-process windows,
transient dialogs, utility windows, override-redirect menus, modal dialogs,
unresponsive close handlers, and windows that change WM_CLASS/title.

## 12. Primary references

- [Extended Window Manager Hints 1.5](https://specifications.freedesktop.org/wm/latest-single/)
- [Inter-Client Communication Conventions Manual](https://www.x.org/releases/current/doc/xorg-docs/icccm/icccm.html)
- [X11 core protocol](https://www.x.org/releases/X11R7.6/doc/xproto/x11protocol.pdf)
- [Desktop Entry Specification](https://specifications.freedesktop.org/desktop-entry/latest-single/)
- [x11rb documentation](https://docs.rs/x11rb/latest/x11rb/)
