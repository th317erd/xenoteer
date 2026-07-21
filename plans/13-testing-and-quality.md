# Testing, verification, and quality gates

## 1. Testing philosophy

Xenoteer coordinates asynchronous systems that can acknowledge a request without
the intended UI effect. Tests must observe the effect through an independent
plane: an X event recorder, window model, AT-SPI state, pixel result, process
state, or API trace. A 2xx response alone is not evidence.

Tests are deterministic by default. Random/property/fuzz tests record seeds and
minimize failures. Time-based tests use explicit generous bounds and event-driven
conditions; fixed sleeps appear only where timing itself is the subject.

## 2. Test layers

### 2.1 Pure unit/property tests

Run on every developer/PR platform without a desktop:

- geometry conversions/intersections/overflow;
- interpolation samples, rounding, delay sums, waypoint allocation;
- input/lease/command/INCR/process state machines;
- selector parsing/evaluation/order/budgets;
- canonical request hash and protocol validation;
- pixel format decode and cursor composition;
- EWMH/ICCCM property decoding;
- AT-SPI cache normalization/compatibility;
- auth/capability/path/config validation;
- event coalescing/replay/overflow;
- error/problem redaction/serialization.

Use proptest for invariant-rich domains, with checked-in regression seeds/cases.
Property generators respect valid ranges and also target boundaries explicitly.

### 2.2 Crate integration tests

Run against ephemeral Xvfb/D-Bus when needed:

- x11rb connection/extension negotiation/barriers;
- XTEST delivery to recorder;
- window EWMH operations under xfwm4;
- clipboard owner/requestor/INCR;
- capture root/window/cursor;
- AT-SPI fixture discovery/actions/events;
- process group launch/termination/reaping;
- Axum protocol/auth/backpressure with fake or real actor handles.

Group related expensive cases in a single integration test binary with modules
and per-test isolated displays. Cargo otherwise runs integration binaries
serially and duplicates setup/build overhead.

### 2.3 Container end-to-end

Black-box tests launch the release image and use only public SDK/API. They prove:

- boot/readiness/shutdown and listener policy;
- launch/query/control/wait/screenshot/viewer workflow;
- toolkit/browser compatibility;
- restart/generation/stale behavior;
- hardened runtime profiles;
- package/license/SBOM contents.

Never mount the source tree or test-only control socket into the container under
e2e. Test fixtures may be image-installed or launched from a dedicated test
image derived from the exact release image digest.

### 2.4 Soak/performance/security

Scheduled and release-candidate jobs cover long duration, load, fuzz, scanners,
and browser/viewer memory behavior. They are not all PR-blocking, but a current
passing run is required for release.

## 3. Test fixture suite

### 3.1 X11 event recorder

Rust test app with a known WM_CLASS/title and visible grid. It records to a
bounded local JSONL/test channel:

- MotionNotify coordinates/time/state;
- ButtonPress/Release detail/time/state;
- KeyPress/Release keycode, resolved keysym, state;
- Enter/Leave, FocusIn/Out;
- Configure/Map/Unmap/Destroy;
- clipboard paste result;
- optional pointer grabs/warps and configurable input delay.

It can paint current state into its window so screenshots independently reflect
events. Test-only output is not a production daemon input.

### 3.2 GTK fixture

Standard and custom GTK controls:

- buttons/double-click area, menus/submenus, toolbar;
- entries, text view, protected entry, selection/caret;
- check/radio/switch, slider/spin, combo;
- tabs, tree/table/list, scroll area/offscreen item;
- file chooser/dialog/modal/transient windows;
- drag/drop targets and hover-sensitive tooltip/menu;
- dynamic add/remove/rename/disable/hide;
- intentionally inaccessible custom drawing area;
- status panel reflecting semantic/physical actions.

### 3.3 Qt fixture

Mirror GTK behaviors with standard Qt widgets, QAccessible custom/absent widgets,
old/new cache behavior as packaged, native menus, model/view tables, dialogs,
and QtWebEngine page.

### 3.4 Web/browser fixture

Locally served, deterministic, no internet dependencies:

- HTML controls, contenteditable/password, ARIA roles/states;
- hover menus, drag/drop, pointerenter/motion counter;
- iframe and shadow DOM as platform accessibility exposes them;
- navigation/reload/history invalidating document references;
- canvas with no useful semantics;
- huge/virtualized accessibility tree generator;
- download/upload and clipboard actions;
- animation toggle and pixel color bars;
- service worker/background behavior only when explicitly tested.

Run in packaged Chromium and Firefox. Electron/QtWebEngine wrapper fixtures cover
their launch/sandbox/accessibility constraints.

### 3.5 Window fixture variants

- size hints/increments/aspect/min/max;
- override-redirect popup;
- transient/modal/group/client leader;
- title/class changes;
- close accepted/refused/delayed/hung;
- pointer grab and warp;
- overlapping, transparent/ARGB, minimized/unmapped;
- process that forks/reparents/changes window PID correlation.

## 4. Deterministic harness

Each desktop test gets:

- unique display number/socket/auth cookie;
- isolated temp home/XDG runtime/config/cache;
- one session bus/AT-SPI bus;
- fixed locale/timezone/fonts/profile;
- reserved ports assigned by harness, not hardcoded collisions;
- process group and cleanup guard;
- deadline for setup/test/teardown;
- artifact directory containing logs/screenshot/model snapshots only on failure or
  explicit request.

Harness waits on protocol readiness, not socket sleeps. Teardown sends graceful
stop, then kills the exact verified process group after bound; it never uses
broad `pkill` patterns.

Parallel tests use separate displays/homes. Tests that mutate global host state
(binfmt, sysctl, Docker daemon) are isolated scheduled jobs, not ordinary suite.

## 5. Input conformance matrix

### Pointer

- 0/1-pixel, horizontal/vertical/diagonal, corners/edges, max signed range;
- instant, linear, smooth, relative, waypoints;
- duration 0/min/default/max, rates 1/60/240;
- duplicates and exact final endpoint/time sum;
- hover-sensitive menu/canvas sees intermediates;
- instant avoids intermediates where requested;
- app warp/interference and endpoint policies;
- queue serialization across two clients/commands.

### Buttons/drag/scroll

- every supported mapping and remapped/missing mapping;
- click/double/triple timing and same position;
- explicit down/up duplicate/unowned validation;
- drag threshold/path/end, app grab, cancel/error/target destroyed;
- button always released on cleanup path;
- vertical/horizontal count, pace, cancellation partial count.

### Keyboard/text

- named key resolution and left/right modifiers;
- chord press/release order and cleanup;
- layout mapping, AltGr, uppercase, non-US;
- current-layout Unicode, unrepresentable failure;
- temporary mapping exact restore and injected failure poison;
- physical/clipboard/AT-SPI/auto strategy reporting;
- emoji/CJK/RTL/combining text and protected-field policy;
- mapping change and externally held modifier.

Record API result, action trace, X events, and fixture UI state and compare all.

## 6. Window and accessibility matrix

### Windows

- initial reconciliation and event updates;
- selector exact/contains/regex/composition/order/ambiguity;
- XID destroy/reuse stale ref;
- activate/focus success/refusal/raw fallback opt-in;
- close accepted/refused/hung, force terminate separate;
- desired state versus toggle dedupe;
- move/resize constraints and frame/client coordinates;
- stacking/workspace one-profile behavior;
- event gap/rebuild/race-free waits.

### AT-SPI

- GTK/Qt/browser application discovery;
- bulk cache and lazy no-cache traversal;
- add/remove/defunct/restart/navigation stale refs;
- selector roles/names/states/interfaces/attributes/scope/ambiguity;
- invoke versus physical click trace/effects;
- focus/value/selection/editable text/scroll;
- offscreen/empty/covered geometry and revalidation;
- missing/custom accessibility honest failure;
- huge/cyclic/malformed tree budgets;
- missing/out-of-order event reconciliation;
- bus loss/reconnect and generation.

## 7. Clipboard/capture/viewer matrix

### Clipboard

All cases in [05](05-keyboard-and-clipboard.md), especially direct/INCR boundary,
owner changes, requestor stalls, target conversions, PRIMARY separation, paste
observation, and value-copy restoration warning.

### Capture

Pixel masks/order/stride, crops/bounds, overlap/minimize, cursor, PNG/raw limits,
SHM parity/fallback, damage coalescing, concurrent capture/input/events.

Visual baselines:

- keyed by exact image digest, architecture, browser/app version, profile;
- compare perceptual/region tolerance for text/theme and exact pixels for
  generated color/pattern fixtures;
- baseline updates require an attached diff and reviewer approval;
- never "fix" flaky pixel tests by broadening global tolerance without root cause.

### Viewer

View-only attempts, cursor, two viewers, WSS/origin/ticket, slow connection,
backend crash/restart, stale generation, and soak. Use browser automation outside
Xenoteer only to drive noVNC test client; verify X recorder receives no viewer
input.

## 8. API/SDK conformance

- Golden protocol JSON and generated schemas;
- server rejects unknown command fields/types and oversize messages;
- SDKs tolerate additive response/event fields;
- REST and WS command behavior equivalent;
- auth/authorization/object scope negative matrix;
- command ledger concurrent duplicates, partial effect, reconnect/restart;
- backpressure, heartbeats, replay/resync, orderly close;
- Rust/TypeScript/Python examples against same image;
- CLI JSON is parseable/stable and stdout/stderr/binary separated;
- token/content redaction in every language error/repr/log hook.

Contract tests are data-driven from `tests/protocol-cases/` so each SDK consumes
identical cases.

## 9. Fault injection

Provide explicit test hooks only in test builds/profile, never hidden production
API. Fault points:

- before/after each input effect stage;
- X protocol error/barrier failure/connection close;
- actor panic/join failure;
- desktop/AT-SPI/viewer process killed;
- D-Bus method timeout/malformed cache item;
- command accepted response lost;
- queue full and worker semaphore exhaustion;
- disk/artifact full/read-only;
- clock wall jump (monotonic deadlines unaffected);
- child ignores SIGTERM/forks/grandchild/output flood;
- clipboard requestor never deletes INCR property;
- WS slow/non-reading client.

Fault assertions include readiness transition, cleanup, process reaping, bounded
memory/time, terminal result/effect stage, and recovery/restart policy.

## 10. Performance and resource testing

Measure on named reference CI hardware and store trends, not only pass/fail.
Initial budgets from subsystem plans plus:

- daemon idle RSS/CPU with desktop ready;
- input admission/queue/execution p50/p95/p99;
- 650 ms move timing error and endpoint mismatch rate;
- window model event lag under churn;
- AT-SPI 10k-node cold/warm query and cache RSS;
- screenshot throughput/latency and event-loop lag;
- WS events/sec under damage/accessibility flood;
- 100 sequential app launch/close zombie count;
- 8/24-hour idle/viewer/browser/action soak with RSS/fd/thread/process trends.

Performance tests warm/cold state explicitly, retain sample distributions, and
fail only statistically meaningful regressions after baseline policy is set.

## 11. Fuzzing and sanitizers

Fuzz targets:

- JSON command/envelope/problem/cursor decode;
- EWMH/ICCCM property decoders;
- X image pixel formats/reply lengths;
- clipboard TARGETS/MULTIPLE/INCR state transitions;
- AT-SPI cache old/new signatures/tree normalization;
- desktop-entry Exec parser;
- selector/regex budgets;
- protocol event replay/coalescing state.

Use structure-aware fuzzing for typed command/state sequences. CI runs a short
corpus pass; scheduled jobs run longer and preserve minimized regressions.

If first-party unsafe is introduced, run Miri on pure unsafe wrappers and
ASan/UBSan where toolchain/dependencies support. Rust memory safety does not
remove logic/DoS/parser testing.

## 12. Static quality gates

PR blocking:

```text
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo doc --workspace --no-deps (warnings denied)
cargo deny check
schema generation diff clean
shell/Dockerfile/config linters
unit + selected Xvfb + container smoke/e2e
```

Review `--all-features` does not enable mutually exclusive production TLS/runtime
features; add explicit feature matrix jobs where it does.

Coverage is reported by crate/domain and used to find gaps, not a single vanity
threshold. Critical state machines, auth, input cleanup, parsers, and errors need
branch coverage expectations and mutation tests where useful.

## 13. CI lanes

### Pull request

- formatting/lints/licenses/advisories/schema;
- unit/property with fixed regression seeds;
- Xvfb/GTK/Qt core integration on amd64;
- build image and black-box smoke;
- protocol/Rust SDK/CLI conformance;
- short fuzz corpus.

### Main/nightly

- Chromium/Firefox/QtWebEngine/Electron matrix;
- hardened read-only/cap-drop/rootless-host profiles;
- full fault injection;
- TypeScript/Python SDK conformance;
- multi-architecture build and native/runtime smoke where runners exist;
- vulnerability/secret/license/image content scans;
- performance trend and 8-hour soak rotation.

### Release candidate

- all nightly gates current;
- 24-hour soak;
- upgrade/rebuild/reproducibility check;
- viewer security and browser sandbox verification;
- SBOM/provenance/signature/source correspondence validation;
- clean-machine quick start and docs examples.

## 14. Flaky-test policy

- A retry may gather evidence but cannot turn a flaky required test green without
  recording the first failure.
- Quarantine requires issue, owner, reason, expiry, and replacement coverage;
  security/input-cleanup/reference tests are not quarantined to release.
- Capture logs, actor health, model snapshots, event tail, process tree, and
  screenshot on first failure under size limits.
- Timeouts are adjusted only after determining observed latency distribution and
  whether readiness/event race is the real problem.
- Tests use event conditions and bounded quiet periods, not arbitrary longer
  sleeps.

## 15. Release quality exit criteria

- Zero known input-stuck or silent-command-duplication defect.
- No critical/high unwaived reachable vulnerability; waivers owned/expiring.
- All supported toolkit/browser fixture workflows pass.
- Browser sandbox and viewer server-side view-only proven.
- Stale/generation/reconnect behavior passes fault matrix.
- No zombie/fd/RSS unbounded trend in soak.
- Public examples and all SDK conformance pass against release digest.
- Image content, SBOM, source/license bundle, provenance, and signature verified.

## 16. Primary references

- [Cargo test command](https://doc.rust-lang.org/cargo/commands/cargo-test.html)
- [Cargo test organization](https://doc.rust-lang.org/cargo/reference/cargo-targets.html)
- [Cargo continuous integration](https://doc.rust-lang.org/cargo/guide/continuous-integration.html)
- [Proptest reference](https://docs.rs/proptest/latest/proptest/)
- [Rust Fuzz Book](https://rust-fuzz.github.io/book/)
- [Structure-aware Rust fuzzing](https://rust-fuzz.github.io/book/cargo-fuzz/structure-aware-fuzzing.html)
