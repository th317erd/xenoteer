# X11 fixtures

This standalone crate is intentionally outside the production workspace.

- `x11-event-recorder` maps a grid window with stable `WM_NAME`/`WM_CLASS` and
  writes bounded JSONL records for motion, buttons, keys, focus, crossing, and
  lifecycle events. Button records include root/window coordinates, allowing
  held-state paths and same-coordinate multi-clicks to be proved without
  trusting the input driver. Key records include the raw keycode and the core
  mapping's first keysym; core `MappingNotify` records retain request type and
  affected keycode range for temporary-keymap diagnostics. The recorder reads
  and caches the complete core map before creating its window, requires bounded
  reply/drain convergence before readiness, and refreshes only after a processed
  Keyboard `MappingNotify`; it never perturbs evidence with a per-key mapping
  request. Use
  `--max-events N` to impose the observed-event bound; readiness metadata and a
  finite diagnostic record for each configured failure control are outside that
  count.
- `x11-color-bars` paints exact red, green, blue, white, and black 100-pixel
  bands. Together with the small raw JSON fixtures, it proves actual GetImage
  storage separately from pure decoder edge cases.
- `x11-input-driver` sends XTEST motion and follows it with `QueryPointer` on
  the same connection. The platform harness checks its endpoint and target
  window against the recorder's independently observed `MotionNotify`.

The first readiness line remains exactly
`{"type":"ready","window":WINDOW}` for Phase 0 consumers. It is followed by
a `ready_metadata` line containing the requested and observed focus, initial
paint completion, pointer-grab state, event limit, and configured post-motion
warp. Both are synchronously flushed. Readiness is emitted only after the first
paint and a reply-producing round trip, so it is a synchronization boundary
rather than merely a MapWindow request.

The recorder initializes its visual state from `QueryPointer` and `QueryKeymap`,
then repaints the pointer position and exact locally tracked button/key sets
after every input transition. Each repaint has a reply-producing round trip
before its input JSONL line, making the line a synchronization boundary for a
test that immediately captures the window.

Recorder controls used by Phase 1 failure tests are:

- `--focus-before-ready`: focus the mapped fixture and verify the observed focus
  in `ready_metadata` before reporting readiness;
- `--post-motion-warp X Y`: after the first recorded motion, warp once to the
  root coordinate and emit requested/observed `pointer_warped` evidence after a
  same-connection `QueryPointer` barrier;
- `--grab-pointer`: establish an asynchronous active pointer grab before
  readiness, failing startup if the server does not grant it;
- `--release-pointer-grab-after-button-press`: explicitly release that grab and
  emit `pointer_ungrabbed` after the first press (requires `--grab-pointer`);
- `--destroy-after-button-press`: record a press, synchronously request fixture
  destruction, then emit `destroy_requested` and the observed `destroy` event;
- `--max-events N` and the retained `--exit-after-motion`: deterministic
  controller-independent termination. An active configured grab is explicitly
  released before these normal exits.

`tests/platform/run-phase1-input.sh` allocates an authenticated isolated Xvfb,
proves these controls, and uses `xdotool` only as an independent fixture oracle
for physical motion/button/key semantics. Production and example code never
call it. The actor integration contract is the Cargo example
`crates/xenoteer-x11/examples/phase1-input.rs` (target `phase1-input`), accepting
`--window WINDOW --scenario conformance`, enabling `native-xkbcommon`, and
writing JSONL results. Its pointer
lane proves smooth intermediate motion and exact endpoint timing, instant
motion, same-coordinate double click timing, held drag waypoints and release,
vertical/horizontal scroll mapping, cancellation at a waypoint boundary, and a
responsive independent `QueryPointer` connection while an XTEST delay remains
in flight. Its keyboard lane proves named/scalar resolution, modifier-first and
reverse-release chord ordering, sequence boundaries, current-layout physical
text, raw keycode identity, keysyms, and modifier state. The extended physical
text case uses a scalar absent from the current layout and requires redacted
actor evidence for exactly one installed temporary mapping, one emitted scalar,
one exact restoration, no disclosed bindings, and aggregate confirmed effects.
An independent connection snapshots every core keysym slot of the proved-safe
reserved key before submission and requires exact equality, nonmodifier status,
and an up `QueryKeymap` bit afterward. The recorder must observe the Unicode
keysym press/release on that same raw keycode strictly bracketed by the
single-key Keyboard `MappingNotify` install and restore records. A missing
example, native keyboard model, action lane, or evidence record fails the gate;
Phase 1 has no construction-time skip path.

Xvfb's XKB compatibility transform canonicalizes the U+2603 probe mapping from
the requested seven-slot `[U+2603, NoSymbol, NoSymbol, ...]` shape to
`[U+2603, NoSymbol, U+2603, NoSymbol, ...]`, duplicating the first core group
into the second. Installed-value verification accepts only that narrow
server-derived duplicate form (or the exact requested form); restoration still
requires complete equality with the original seven-`NoSymbol` snapshot. The
public action outcome remains text-redacted: the fixed scalar and raw mapping
shape exist only in the conformance input and independent fixture evidence.

Xvfb lazily broadcasts one complete Keyboard/Modifier mapping pair when the
actor's XTEST connection processes its first key, even after checked XKB
negotiation. The conformance example handles that server-specific behavior as
an explicit caller-requested diagnostic, never as hidden actor startup input:
it proves a keycode is wholly `NoSymbol`, absent from the modifier map, and up
in `QueryKeymap`, submits one typed raw press, and accepts only normal completion
or `KeyboardMappingChangedAfterEffect` with known two-event progress and proved
cleanup. All named/scalar/chord/text assertions occur afterward and remain
strict. This diagnostic does not alter the ordinary actor contract and is not a
retry of an ambiguous semantic action.

Both use the display and authentication already present in `DISPLAY` and
`XAUTHORITY`; neither disables X authentication or opens TCP.

The fixture crate has its own lockfile because it is intentionally outside the
production workspace. Run its canonical local quality gate from the repository
root:

```sh
cargo fmt --manifest-path fixtures/x11/Cargo.toml --all --check
cargo clippy --manifest-path fixtures/x11/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path fixtures/x11/Cargo.toml --all-targets --locked
cargo deny --manifest-path fixtures/x11/Cargo.toml --all-features --locked check
```

The last command deliberately reuses the repository-root `deny.toml`. CI runs
these commands independently of the production workspace gates so fixture-only
dependency or lockfile changes cannot bypass review.
