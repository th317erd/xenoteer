# AT-SPI semantic automation

## 1. Goal and truth model

AT-SPI provides application-supplied semantic objects: roles, names, states,
relationships, actions, text, values, and component bounds. It is powerful but
not a DOM and not a pixel truth source.

Xenoteer treats the accessibility tree as one observation plane:

- an accessible object can be stale, defunct, incomplete, duplicated, offscreen,
  or geometrically inconsistent with current pixels;
- custom canvas/OpenGL widgets may expose no useful children;
- toolkit adapters differ in interfaces, cache signatures, event quality, and
  activation requirements;
- browser trees can be enormous and accessibility has runtime cost.

Semantic calls therefore return evidence and typed limitations. Physical input
and screenshots remain independent fallbacks selected explicitly by the caller.

## 2. Runtime architecture

`AtspiActor` is an async task set using the Rust `atspi` crate with zbus's Tokio
integration (`default-features=false`, `tokio` enabled). It owns:

- accessibility bus connection lifecycle;
- desktop/application discovery;
- event listener registration and normalization;
- bounded application/object cache;
- selector evaluation and snapshots;
- semantic action requests;
- correlation with X11 windows/processes.

Connection loss invalidates semantic references and increments an
`atspi_generation`. If it implies a whole desktop restart, desktop generation
also changes. The actor reconnects with capped backoff, rebuilds from desktop
roots/cache, emits `accessibility.resync_required`, and remains degraded until
the probe succeeds.

## 3. Reference model

### 3.1 ElementRef

```text
ElementRef {
  desktop_id,
  desktop_generation,
  atspi_generation,
  application: { unique_bus_name, root_object_path, app_instance_generation },
  object_path,
  object_identity_hash,
  cache_sequence
}
```

AT-SPI's object reference is normally `(bus name, object path)`. Unique bus names
can disappear and paths can be reused after application restart. The extra
generations and identity evidence ensure old references fail as stale.

`object_identity_hash` may incorporate stable accessible ID when provided,
initial role/name/parent path, and first-observed sequence. Name/role are evidence,
not standalone identity; they can legitimately change.

### 3.2 Validation

Before any operation:

1. validate desktop and AT-SPI generations;
2. validate application bus ownership/instance generation;
3. find/re-query the object path;
4. reject defunct state or missing object;
5. compare identity evidence under a per-operation strictness policy;
6. refresh interfaces/states required by the operation.

Never auto-run the original selector on a stale reference. That could click a
different "OK" button in a replacement dialog.

## 4. Cache and discovery

### 4.1 Initial load

Discover the AT-SPI desktop and application roots. Prefer the standard Cache
interface's bulk `GetItems` when implemented, because individual GetChildren and
property calls create heavy D-Bus traffic. Then update from Cache
`AddAccessible`/`RemoveAccessible` and normalized object/window events.

Qt may still expose the older Cache item signature. The Rust adapter must accept
only signatures supported by the selected crate or include a compatibility
decoder with fixture tests; an unexpected signature is a per-application cache
degradation, not daemon memory corruption.

Applications without usable Cache are traversed lazily with budgets.

### 4.2 Cache entry

```text
AccessibleNode {
  ref,
  parent_ref?, index_in_parent?, child_count?,
  interfaces,
  role, name, description,
  states,
  attributes?, relations?,
  component_extents?,
  window_correlation?,
  revision, last_refresh, completeness, warnings
}
```

The cache stores only configured common fields eagerly. Text contents, full
attributes, relations, actions, table cells, and large child lists are fetched
on demand. Entries have LRU/TTL bounds and pinning while a command uses them.

Initial per-desktop limits:

- 100,000 cached nodes;
- 25,000 nodes visited per query;
- selector depth 64;
- returned matches 1,000;
- snapshot nodes 10,000 / encoded 16 MiB;
- per-proxy call timeout 2 seconds;
- whole query default timeout 10 seconds.

When a browser page exceeds limits, return a truncated snapshot/query with a
continuation cursor only where traversal order is stable; otherwise return
`query_budget_exceeded` and recommend a narrower scope.

### 4.3 Reconciliation

Events can be missing or out of order. The cache marks affected subtrees dirty
and lazily reconciles them. A parent/child contradiction never causes infinite
recursion: traversal tracks visited `(bus,path)`, detects cycles, and records
`malformed_tree` warnings.

Full rebuild triggers:

- bus reconnect;
- application name owner change;
- event gap/overflow;
- cache signature/protocol mismatch;
- child/parent contradiction above threshold;
- explicit diagnostic request.

## 5. Public semantic snapshot

Return a normalized, protocol-owned model rather than serializing crate/zbus
types directly. Fields:

- `ElementRef`, parent, index, child count;
- normalized stable role enum plus raw numeric/name for forward compatibility;
- name, description, accessible ID, locale;
- states (enabled, sensitive, focusable, focused, visible, showing, editable,
  selected, checked, expanded, defunct, protected, etc.);
- supported semantic interfaces;
- root-physical rectangle and source coordinate system;
- action names/descriptions/keybindings;
- value bounds/current/increment;
- bounded text metadata or content only when requested/authorized;
- attributes and relationships under explicit expansion flags;
- correlated `WindowRef` and confidence/evidence;
- completeness/truncation/warnings/revision.

Protected text content is redacted. Snapshots do not include full document text
by default. Strings and arrays have per-field and aggregate limits.

## 6. Selectors

Selectors combine normalized predicates:

- role(s);
- exact/contains/prefix/regex name or description;
- state required/forbidden;
- interface required;
- attribute key/value;
- accessible ID;
- action name;
- value range;
- visible/showing/focusable/editable;
- within a parent/subtree/window/application;
- relation to another element;
- nth/index and result ordering.

Selector evaluation has deterministic traversal (`preorder` by cache child
index, with path tie-break). Single-target actions fail ambiguity unless the
caller explicitly supplies an ordering/selection policy.

Avoid browser-CSS terminology. Names such as `query`, `find`, `role`, and
`subtree` make the abstraction honest. A future browser-specific adapter can
offer CSS separately.

## 7. Application and window correlation

Correlation uses multiple weak signals:

- AT-SPI application process ID versus `_NET_WM_PID`;
- component top-level extents versus X window rectangles;
- AT-SPI window title/name versus EWMH title;
- toolkit/application identity and client leader;
- focus events on both planes;
- temporal creation proximity.

Return confidence (`exact_process`, `strong`, `weak`, `none`) and evidence. Never
authorize or silently choose a physical target solely from a weak title match.
Physical `element.click` requires strong correlation by default or an explicit
window reference supplied by the caller.

## 8. Semantic actions

### 8.1 `element.invoke`

1. validate reference and Action interface;
2. refresh states and reject defunct/disabled/insensitive unless override;
3. list action names and resolve requested action by exact normalized name or
   explicit index;
4. invoke `DoAction` with timeout;
5. await caller-supplied postcondition/event when present;
6. return action resolution, D-Bus result, events observed, and final snapshot.

Default action selection may use a toolkit-reported `click`/`press`/`activate`
preference table, but it must report the concrete action and fail ambiguity.
Do not generate a mouse event from `invoke`; upstream AT-SPI guidance itself
prefers Action over synthetic mouse when semantic invocation is intended.

### 8.2 `element.click` (physical)

1. validate element and correlated window;
2. require Component interface, showing/visible/sensitive state under policy;
3. query current extents in screen coordinates immediately before input;
4. intersect with root and optionally window client rectangle;
5. choose point by explicit policy: center default, inset center, caller offset,
   or nearest visible point;
6. optionally call AT-SPI `ScrollTo`/`ScrollToPoint` when offscreen and wait for
   showing/extents change;
7. activate correlated window and verify focus;
8. revalidate element revision/extents after queue delay;
9. send interpolated physical click through InputActor;
10. await optional semantic/window/visual postcondition.

If extent is empty/offscreen/covered, return a typed error. Occlusion detection
can compare X stacking/rectangles but cannot understand arbitrary shaped/
transparent windows perfectly; report `occlusion_check=best_effort`.

### 8.3 Focus, value, selection, text

- `focus`: use Component `GrabFocus`, then wait for focused state and X focus
  correlation when applicable.
- `set_value`: validate Value interface/range, request value, wait/read back with
  tolerance appropriate to increment; do not drag a slider physically.
- `select`: use Selection interface and verify selected state.
- `set_text`/`insert_text`: use EditableText under authorization, preserve or
  set caret/selection according to explicit options, and verify event/readback.
- `scroll_to`: use Component semantic method and report extents before/after.

Each is a semantic method. Equivalent physical workflows remain separate.

## 9. Event normalization and waits

Subscribe only to event families needed by active capabilities to avoid D-Bus
floods. Normalize at least:

- object state/property/children/active-descendant changes;
- focus and window activate/deactivate/create/destroy;
- text inserted/deleted/caret/selection changes;
- value and selection changes;
- bounds/visible-data changes;
- Cache add/remove.

Event envelope carries source `ElementRef` when resolvable, raw bus/path,
normalized type, details, revision/sequence, and `source_stale` when resolution
failed. Sensitive text event payloads are redacted by default.

Race-free waits use snapshot-sequence-subscribe-recheck as in the window plan.
Supported predicates include exists/gone, state, name/value/text (authorized),
focus, child count, geometry, and custom selector count. Poll fallback is bounded
and disclosed when a toolkit fails to emit the expected event.

## 10. Toolkit/browser compatibility

### GTK

Standard widgets should expose AT-SPI through GTK's accessibility stack. Custom
widgets must implement accessible roles/properties; otherwise use physical/
visual fallback. GTK 3 and GTK 4 can differ in cache and role structure, so
fixtures cover both if both ship in the image.

### Qt

Qt's AT-SPI bridge activates only when both `org.a11y.Status.IsEnabled` and
`ScreenReaderEnabled` are true, or when
`QT_LINUX_ACCESSIBILITY_ALWAYS_ON` is set. The image sets both mechanisms. Qt
custom widgets need QAccessible implementations. Test old/new Cache signatures.

### Chromium/Electron

Chromium accessibility is normally on demand. Launch with
`--force-renderer-accessibility=complete`. Expect a browser chrome tree plus
per-document subtrees and frequent replacement on navigation. Do not treat an
object path as stable across document reload. Electron can enable accessibility
through its app API, but third-party apps may need the Chromium flag.

### Firefox

Use the image's accessibility-enabled profile and verify an application/document
tree at runtime. Multi-process content objects can be replaced after navigation.
Version-specific preferences remain image-profile implementation details, not a
public guarantee.

### Canvas/custom rendering

Canvas, terminal grids, games, remote desktops inside the desktop, and custom
OpenGL widgets may expose one coarse node or none. Return honest capability and
allow root/window physical automation. OCR/image matching is deferred, not
secretly approximated through accessibility names.

## 11. Error model

- `accessibility_unavailable`
- `application_not_accessible`
- `element_not_found`
- `ambiguous_target`
- `stale_reference`
- `element_defunct`
- `interface_not_supported`
- `element_not_showing`
- `element_not_sensitive`
- `element_not_editable`
- `action_not_found`
- `weak_window_correlation`
- `element_geometry_invalid`
- `element_occluded`
- `query_budget_exceeded`
- `snapshot_truncated`
- `toolkit_protocol_error`
- `semantic_postcondition_failed`

Errors may return a bounded final snapshot and strategy alternatives; they never
silently fall back to physical input.

## 12. Testing

### Fixture applications

Create controlled GTK and Qt apps with buttons, menus, tabs, trees, tables,
editable/protected text, sliders, checkboxes, radio groups, dialogs, transient
objects, offscreen scroll content, intentionally malformed/custom widgets, and
rapid add/remove churn. Browser fixture pages mirror these controls plus iframe,
shadow DOM (as exposed through platform a11y), navigation, large virtualized
lists, canvas, and ARIA errors.

### Required cases

- cache bulk load, incremental add/remove, rebuild, old Qt signature;
- reference stale after object removal, app restart, page reload, bus reconnect;
- cycle/bad parent/huge tree budgets;
- selector determinism, ambiguity, regex/input limits;
- invoke versus physical click produces distinct recorded evidence;
- semantic focus/value/selection/editable text and protected-field policy;
- scroll-to and geometry revalidation before click;
- weak/strong window correlation;
- missing events trigger bounded poll fallback;
- event flood/slow subscriber causes coalesce/resync, not daemon OOM;
- Chromium/Firefox navigation invalidates document references;
- accessibility disabled/missing adapter degrades without blocking physical API.

Performance gates include bounded cold snapshot time for 10k nodes, selector p95,
event lag under churn, and stable cache RSS during large-browser soak.

## 13. Primary references

- [AT-SPI architecture](https://gnome.pages.gitlab.gnome.org/at-spi2-core/devel-docs/architecture.html)
- [AT-SPI Cache interface](https://gnome.pages.gitlab.gnome.org/at-spi2-core/devel-docs/doc-org.a11y.atspi.Cache.html)
- [AT-SPI Collection interface](https://gnome.pages.gitlab.gnome.org/at-spi2-core/devel-docs/doc-org.a11y.atspi.Collection.html)
- [libatspi API](https://gnome.pages.gitlab.gnome.org/at-spi2-core/libatspi/index.html)
- [Rust `atspi` crate](https://docs.rs/atspi/latest/atspi/)
- [zbus Tokio configuration](https://docs.rs/zbus/latest/zbus/)
- [Qt QAccessible activation](https://doc.qt.io/qt-6/qaccessible.html)
- [GTK accessibility guidance](https://docs.gtk.org/gtk4/section-accessibility.html)
- [Chromium accessibility overview](https://chromium.googlesource.com/chromium/src/%2B/HEAD/docs/accessibility/overview.md)
- [Chromium accessibility inspection tools](https://chromium.googlesource.com/chromium/src/%2B/125.0.6422.112/tools/accessibility/inspect/README.md)
- [Electron accessibility](https://www.electronjs.org/docs/latest/tutorial/accessibility/)
- [Firefox accessibility architecture](https://firefox-source-docs.mozilla.org/accessible/Architecture.html)
