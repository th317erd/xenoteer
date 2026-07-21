# Keyboard, text, and X11 selections

## 1. Scope and core distinction

Keyboard automation has two different promises:

- **physical key input** emits XTEST keycodes and modifier transitions;
- **text insertion** delivers an exact Unicode string using a declared strategy.

The API never labels clipboard paste or AT-SPI EditableText mutation as typing.
Traces and results identify the selected strategy and any clipboard/keymap side
effects.

Clipboard is not stored centrally by X11. CLIPBOARD and PRIMARY are global
selections owned by clients that must answer `SelectionRequest` events. Xenoteer
therefore needs a continuously serviced selection actor; setting bytes in a
property and exiting is not a clipboard implementation.

## 2. Keyboard architecture

The input actor owns key injection and pressed-key state. A `KeyboardModel`
owned by that actor contains:

- X server minimum/maximum keycodes;
- current XKB keymap and state;
- keysym-to-keycode/level/modifier candidates;
- current group/layout and modifier mapping;
- mapping generation updated on XKB/core MappingNotify;
- reserved unused keycode candidates for explicit temporary remapping.

Use libxkbcommon's X11 integration to construct keymap/state from the core
keyboard device. XTEST still receives keycodes; xkbcommon is the model, not the
injection backend.

### Startup checks

1. Query XKB extension/version and core keycode range.
2. Build keymap from the server, not from assumed `us` files alone.
3. Verify required named control keys exist.
4. Select keyboard mapping/state notifications.
5. Record configured layout/variant/options and keymap fingerprint.
6. Refuse physical text strategies if the model cannot match server generation.

## 3. Public key model

### 3.1 Key identifiers

Accept a closed, versioned set of logical names (`Enter`, `Tab`, `Escape`,
`ArrowLeft`, `F1`, `ControlLeft`, and so on), printable Unicode scalar values,
and an advanced raw keycode form gated by capability. SDK enums are generated
from protocol schema.

Distinguish left/right modifiers. Generic `Shift` resolves to configured left
default but reports the concrete keycode. Unknown strings are validation errors,
not passed through to xkbcommon heuristics.

### 3.2 Operations

- `key.down(key)`
- `key.up(key)`
- `key.press(key, hold_ms)`
- `key.chord(keys, hold_ms)`
- `key.sequence(steps, interval_ms)`
- `keyboard.reset_owned()`
- `text.insert(text, strategy, options)`

Raw down/up require the active lease. Duplicate down or unowned up is rejected
unless diagnostic `allow_redundant` is set. Press/chord are atomic with cleanup.

### 3.3 Modifier behavior

For a chord, press modifiers in caller order, then non-modifiers; release in
reverse order. If a text mapping needs temporary modifiers:

1. snapshot actor-owned and observed modifier state;
2. never release a modifier Xenoteer does not own merely to reach a level;
3. press missing required owned modifiers;
4. emit the key;
5. release only modifiers added by this operation;
6. verify through a reply barrier and XKB state event/query.

If an externally held modifier makes exact physical text impossible, return
`conflicting_modifier_state` or choose another declared `auto` strategy. Do not
silently clear external state.

## 4. Physical key execution

### 4.1 Press

1. resolve key under the current mapping generation;
2. send required modifier presses, recording owned state;
3. send key press;
4. send key release with `hold_ms` delay, default 35 ms;
5. release temporary modifiers in reverse order;
6. issue same-connection barrier;
7. return concrete keycodes/keysyms/modifiers and effect stage.

If any request after a press fails, cleanup releases the key and temporary
modifiers. The global reset policy in [03](03-input-engine.md) applies.

### 4.2 Sequence

Sequences are arrays of complete presses or explicit down/up steps. Complete
presses are cancellation boundaries; explicit down/up sequences are treated as
one advanced atomic action and require a final state assertion. Limits:

- at most 10,000 steps;
- per-delay 0..10,000 ms;
- total planned duration at most 5 minutes by default;
- request/body and queue limits still apply.

Progress reports include completed step count. A timeout after effects returns
the partial count and is never automatically retried.

### 4.3 Mapping changes

On mapping notification, finish the current atomic operation using its captured
resolution, then rebuild before accepting the next resolution-dependent action.
If generation changes during an explicit multi-key physical text action, stop at
the next complete character, restore owned modifiers, and return
`keymap_changed` with inserted character count.

## 5. Exact text strategies

Public strategies:

- `physical`: current-layout keycodes only; fail on an unrepresentable scalar.
- `physical_extended`: current layout, then serialized temporary key mapping;
  opt-in because it mutates global server mapping.
- `clipboard`: own CLIPBOARD, send paste chord, and observe transfer.
- `atspi`: replace/insert through EditableText when the target supports it.
- `auto`: policy engine described below.

Input text is validated UTF-8, max 1 MiB by default, and kept out of ordinary
logs. Results count Unicode scalar values and UTF-8 bytes; they do not claim
grapheme counts unless computed explicitly.

### 5.1 Current-layout physical text

For each Unicode scalar:

1. map the scalar to its X11 Unicode keysym when needed;
2. find candidate `(keycode, group, level, required_modifiers)` pairs;
3. prefer current group, fewest added modifiers, no locking modifier changes,
   and deterministic lowest keycode tie-break;
4. emit a complete press with paced interval, default 5 ms;
5. stop and return precise index on mapping/interference failure.

Do not assume a US layout even though the default image profile uses one. Do not
use locale-dependent Compose/dead-key sequences unless a future tested strategy
explicitly models their state. A sequence that visually produces a character on
one layout can have different application semantics from a direct keysym.

### 5.2 Temporary key mapping

`physical_extended` handles otherwise unrepresentable Unicode keysyms:

1. require exclusive bot control and server-side VNC view-only mode;
2. choose a keycode whose complete mapping is `NoSymbol`, is not a modifier, is
   not pressed, and is reserved by the actor at startup;
3. save its complete mapping and the current mapping generation;
4. change the mapping for that one keycode to the desired keysym at level zero;
5. flush and use a reply barrier; allow clients to receive MappingNotify;
6. emit press/release for the temporary keycode;
7. restore the exact saved mapping in a `finally`-style actor state transition;
8. barrier again and rebuild the keyboard model;
9. poison input if restoration cannot be proven.

The operation serializes all input and is slow. It is not selected by `auto` in
release one unless configured `text.auto.allow_temporary_keymap=true`. It cannot
run if no genuinely unused keycode exists. Multiple scalars may reuse one
reservation, but mapping is restored between characters initially to minimize
global disruption; batching is a measured future optimization.

### 5.3 AT-SPI EditableText

AT-SPI text insertion requires an explicit element target and the EditableText
interface. Semantics are application/toolkit actions, not physical input. The
method validates the element reference, focus/editability/sensitivity state,
applies insert/replace policy, then reads back an allowed text range or waits for
a text-changed event.

Password/protected fields are denied by default to prevent readback/logging
surprises; an authorized write-only mode may invoke the operation without
returning content. See [07](07-atspi-semantics.md).

### 5.4 Auto policy

`auto` is deterministic and reported:

1. If an explicit accessible editable element is supplied and semantic mutation
   is allowed, use AT-SPI.
2. Else if all scalars are representable in current layout and physical typing
   is within the size/duration budget, use physical.
3. Else if clipboard use is allowed and target focus can be verified, use
   clipboard.
4. Else if temporary mapping is explicitly enabled, use physical_extended.
5. Else return `no_text_strategy` with reasons for each rejected strategy.

Auto never changes policy midway through a string by default. A mixed strategy
could produce surprising undo/input events. Callers can split text explicitly.

## 6. Clipboard/selection actor

### 6.1 Connection and event loop

`ClipboardActor` owns a separate X connection and a hidden InputOnly owner
window. It continuously handles:

- `SelectionRequest`, `SelectionClear`, `SelectionNotify`;
- `PropertyNotify` for INCR handshakes;
- XFIXES selection-owner notifications;
- commands from the daemon to get/set/clear/paste selections.

It must remain responsive while the input connection is delayed. Its bounded
command queue and X event loop are multiplexed without starving requests; a
selection requester is an external client with a protocol timeout.

### 6.2 Selections are separate

Support `CLIPBOARD` and `PRIMARY` as independent stores/owners. Never mirror
PRIMARY to CLIPBOARD automatically. SECONDARY is observable but unsupported for
set/paste in release one.

Each owned selection has:

```text
SelectionState {
  selection,
  acquired_server_time,
  revision,
  representations: map<TargetAtom, Bytes>,
  source: Api | TemporaryPaste | RestoredSnapshot,
  active_incr_transfers,
  expiry_policy
}
```

### 6.3 Required targets

For owned textual data, serve:

- `TARGETS`
- `TIMESTAMP`
- `MULTIPLE`
- `UTF8_STRING`
- `text/plain;charset=utf-8`
- `text/plain`
- `STRING` only when lossless Latin-1 conversion is possible

`COMPOUND_TEXT` encoding/decoding is deferred in release one: implementing it
incorrectly is worse than an explicit unsupported conversion, while modern
clients have `UTF8_STRING`. A future codec is capability-gated and must pass
legacy-client fixtures before being advertised.

For binary clipboard API data, initially permit `image/png` and explicitly
configured MIME atoms. Never deserialize images in the daemon merely to serve
them. Target names and payload sizes are bounded.

Unknown/unsupported conversion requests receive `SelectionNotify` with property
`None` as ICCCM requires. Requests using a `None` property are supported through
the obsolete-client fallback only after a compatibility test; otherwise fail
safely and document it.

### 6.4 INCR transfers

Payloads above the safe single-property threshold, initially 256 KiB, use ICCCM
INCR:

1. place total lower-bound size as type INCR on requestor property;
2. notify requestor;
3. wait for requestor to delete the property;
4. write chunks, initially 64 KiB, after each deletion;
5. finish with a zero-length property of the real target type;
6. time out and clean transfer state after 10 seconds or requestor destruction.

Maximum selection payload is initially 16 MiB. Active INCR transfers are limited
to two per requestor and eight globally. MULTIPLE items are processed in property
order and fail independently, as ICCCM specifies; MULTIPLE is not transactional.

The wire contract carries text/binary data inline only through 256 KiB. Larger
`selection.set` or `text.insert` values are uploaded as a private immutable
`clipboard_input` artifact and the command carries its `ArtifactRef`, content
type, length, and SHA-256. The daemon rechecks owner, purpose, hash, length,
expiry, and 16 MiB ceiling at command execution. Clipboard reads return inline
bytes through the same threshold and otherwise create a private expiring output
artifact. This keeps 16 MiB data out of 1 MiB JSON/WebSocket envelopes without
changing the selection engine's payload semantics.

### 6.5 Clipboard get

To read another owner's selection:

1. observe owner and server timestamp;
2. request TARGETS to choose a preferred supported representation;
3. request data into a unique property on the hidden window;
4. support direct and INCR replies;
5. verify owner/revision did not change during transfer;
6. enforce target, payload, chunk, and timeout limits;
7. return bytes, target, owner-change warnings, and content hash—not content in
   logs.

Preferred text order is UTF8_STRING, `text/plain;charset=utf-8`, `text/plain`,
then STRING. Invalid encoding returns an error with raw bytes only when the
caller requested a binary response.

### 6.6 Clipboard-assisted paste

The operation coordinates ClipboardActor and InputActor:

1. verify/activate target window or editable element.
2. If preservation is requested (default), snapshot the current CLIPBOARD text
   representation within a 4 MiB preservation budget and record whether no
   owner existed. Exact restoration of every arbitrary MIME target is not
   promised.
3. Take CLIPBOARD ownership with the requested UTF-8 text and verify ownership.
4. Send the platform paste chord, default `ControlLeft+V`, through InputActor.
5. Wait for at least one compatible SelectionRequest and completion of any INCR
   transfer, then an initial 50 ms quiet window; cap at 2 seconds.
6. If a semantic/visual postcondition was supplied, evaluate it independently.
7. Restore the captured text value by remaining as a Xenoteer owner, or
   relinquish ownership if there was no owner/value. X11 cannot restore the
   original owner process; report `restoration_kind=value_copy`.
8. Return whether a paste request was observed, targets requested, transfer
   result, postcondition result, and restoration result.

If no SelectionRequest is observed, return `paste_not_observed` after restoring.
Never assume the shortcut means data was consumed. `preserve=false` leaves
Xenoteer owning the new value and is the faster explicit option.

An application may request clipboard data later than two seconds. Reliable
automation should supply a target-specific postcondition; the default timeout is
a practical bound, not proof of application state.

## 7. Security and privacy

- Text/clipboard contents are secret by default: no ordinary logging, metrics,
  panic reports, or error messages contain them.
- Diagnostic content capture requires an explicit trace policy and redaction
  budget.
- Clipboard read/write and protected-field semantic insertion are separate
  authorization capabilities.
- Selection targets and lengths are untrusted external input and are fuzzed.
- Reject target atoms outside configured character/length rules for API set;
  observed existing atoms may be reported as escaped names.
- Clear temporary paste payload memory promptly after restoration; Rust cannot
  guarantee allocator-wide zeroization, so do not claim it does.

## 8. Error categories

- `key_not_found`
- `key_not_representable`
- `keymap_changed`
- `conflicting_modifier_state`
- `temporary_keymap_unavailable`
- `temporary_keymap_restore_failed` (poisons input)
- `selection_has_no_owner`
- `selection_target_unsupported`
- `selection_changed_during_transfer`
- `selection_too_large`
- `selection_transfer_timeout`
- `paste_not_observed`
- `clipboard_restore_partial`
- `no_text_strategy`

All partial-effect errors include completed scalar/step count and effect stage.

## 9. Test matrix

### Keyboard

- left/right modifiers, chords, repeats, paced sequence, held-key reset;
- US and at least one non-US layout profile;
- upper/lowercase, AltGr, combining characters, emoji, RTL text, CJK;
- externally held modifier conflict;
- mapping notification between characters;
- unused-keycode selection, remap, restore, and injected restore failure;
- application event recorder verifies keycodes, keysyms, state, and order.

### Clipboard

- independent PRIMARY/CLIPBOARD values;
- no owner, owner disappears, owner changes mid-transfer;
- TARGETS/TIMESTAMP/MULTIPLE success and partial failure;
- UTF8, Latin-1 fallback, invalid data, image/png bytes;
- direct threshold boundaries and multi-chunk INCR both directions;
- malicious sizes, missing property deletes, destroyed requestor, concurrent
  requestors, global transfer limit;
- GTK, Qt, Chromium, Firefox, and terminal paste consumption;
- preserve/restore value, no-owner restoration, partial-format warning;
- daemon remains responsive during long XTEST delay and INCR transfer.

## 10. Primary references

- [ICCCM selection protocol and INCR](https://www.x.org/releases/current/doc/xorg-docs/icccm/icccm.html)
- [Freedesktop Clipboard Specification](https://specifications.freedesktop.org/clipboard/latest/)
- [XTEST keycode semantics](https://www.x.org/releases/X11R7.6/doc/xextproto/xtest.html)
- [libxkbcommon documentation](https://xkbcommon.org/doc/current/index.html)
- [libxkbcommon compose/dead-key support](https://xkbcommon.org/doc/current/group__compose.html)
- [xkbcommon X11 state API](https://xkbcommon.org/doc/current/structxkb__state.html)
- [xdotool upstream reference implementation](https://github.com/jordansissel/xdotool)
