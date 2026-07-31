# Phase 2 desktop-application acceptance image

This Dockerfile layers pinned GTK3, Qt6, QtWebEngine, Chromium, Firefox ESR,
Electron, AT-SPI inspection, and window-inspection packages onto the production
desktop image. It is deliberately labeled `test-only-non-distributable`: its
package/artifact closure is exercised in CI but is not part of the release
runtime contract or the production final-image inventory.

Build the current production image first, then build the fixture image:

```sh
sudo scripts/container/build.sh
sudo scripts/container/build-desktop-app-fixture.sh
sudo scripts/container/test-desktop-app-image.sh xenoteer:desktop-apps-test
sudo scripts/container/test-phase4-live-fixtures.py xenoteer:desktop-apps-test
sudo scripts/container/test-phase5-atspi-live.py xenoteer:desktop-apps-test
```

Both builds use the Debian snapshot recorded in `container/locks/release.lock`.
Every direct acceptance package is version-pinned in `packages.lock`; apt still
resolves their dependency closure from that immutable snapshot. Electron is not
available as a Debian trixie runtime package, so `artifacts.lock` records the
official stable v43.1.1 Linux x64 release URL and SHA-256. The build verifies the
official checksum before extraction and retains Electron's `LICENSE` and
`LICENSES.chromium.html`; no Electron bytes enter the production image.

The build wrapper admits the production tag only when Docker reports an
immutable `sha256:` image ID and a durable non-dangling tag or digest. It gives
`FROM` a strongly random temporary local alias and anchors `--iidfile` in a
private mode-0700 directory. The child path must be absent immediately before
Docker creates it; the wrapper securely validates that child and reduces its
permissions to mode 0600 before reading it. It records the base ID in the
derived image label. On a classic image store the IID must equal the tagged
output's exact image ID. On a containerd image store the IID must instead equal
the `config.digest` annotation in the tagged output's manifest descriptor,
whose digest must equal the tagged output's exact image ID. The wrapper compares
the exact rootfs-layer prefix using only that frozen exact output ID. The
producer fixes `--provenance=false` and `--sbom=false` so the local output stays
one directly inspectable platform manifest. The only caller build options are
at most one each of `--platform`, `--builder`, `--cpu-period`, `--cpu-quota`,
and `--memory`, plus at most one value-free `--no-cache`; split and
`--option=value` forms are accepted only with validated, bounded values:
`--platform` is `local` or OCI `os/arch[/variant]`, with every component 1–32
characters, beginning with a lowercase letter or digit and otherwise containing
only lowercase letters, digits, `_`, `.`, and `-`; `--builder` is a 1–128
character Docker name made from letters, digits, `_`, `.`, and `-`, beginning
with a letter or digit;
`--cpu-period` is 1000–1000000; `--cpu-quota` is 1–1000000000; and `--memory`
is positive bytes or a `b`, `k`, `m`, or `g` quantity no greater than 1 TiB.
A bare `--`, positional context, duplicates, unknown controls,
`-t`/`--tag`, `-f`/`--file`,
`--iidfile`, `-o`/`--output`, push/load/export controls, and all
`--provenance`, `--sbom`, or `--attest` forms are rejected before Docker. The
wrapper exclusively owns the repository context, output tag, IID file,
Dockerfile, exporter/load behavior, internal build arguments and labels, and
attestation policy. OCI indexes and Docker manifest lists remain rejected
because their top-level descriptors do not bind the build IID to one image
config.
Post-proof label checks never resolve the mutable output tag. The alias is
collision-checked, continuously bound to the exact source, and removed only
while it still has the expected identity and another durable source reference
exists. The live matrix again resolves the derived tag once, uses only its
immutable ID, checks the recorded base is locally present, and repeats the
ancestry proof. The test image inherits the production entrypoint, desktop
graph, profiles, and seccomp contract.

The live gate boots `bare`, `standard`, restarted-persistent-HOME, and hardened
read-only-root containers. It requires the exact XFCE process set for each
profile, one workspace, disabled compositing/blanking, an empty ephemeral session
directory, and exactly three TCP listeners: public control port 8080 plus
loopback-only RFB 5900 and viewer WebSocket 6080. GTK3 and Qt6 cover named
menus/toolbars, editable and protected text, toggles, choices, tabs, virtualized
lists, custom areas, disabled controls, and main/transient windows. The local
HTML fixture adds native/ARIA forms, contenteditable text, hover/menu and
drag/drop targets, an iframe, shadow DOM, a 64-item tree, canvas, animation, and
multilingual fonts. It runs in Chromium, Firefox ESR, QtWebEngine, and Electron
with browser-specific AT-SPI markers.

Every matrix container uses Docker's `none` network mode and proves loopback
works while a public TCP endpoint is unreachable. This also avoids the random
`127.0.0.11` DNS listener injected by an internal bridge, preserving the exact
idle listener contract. Each browser runs while a
fixture allocates, maps, and touches exactly 512 MiB of `/dev/shm`; the test
requires at least 480 MiB of measured shm consumption. Browser subprocesses must
remain UID 1000, use seccomp mode 2 plus no-new-privileges, omit sandbox/shared-
memory escape flags, and pass the 4 GiB shared-memory doctor. Chromium's own
`chrome://sandbox` report must show PID/network namespaces and Seccomp-BPF.
Electron must expose neither Node's `process` nor `require` in its file renderer,
and its `/proc` evidence must show seccomp, no-new-privileges, and nested PID and
namespace isolation; QtWebEngine must show a nested PID namespace. Each AT-SPI marker and
process tree must disappear before the exact idle-process auditor accepts the
next fixture, including an explicit QtWebEngine-child check. The hardened run
retains the same sandbox; the gate never substitutes a disabling workaround.

Hardened capability inspection requires `CapInh` and `CapAmb` to be zero for
every browser process, and `CapPrm`/`CapEff` to be zero for every main, renderer,
and non-zygote helper. Chromium-family namespace-sandbox zygotes are the sole
exception: they may hold only `CAP_SYS_ADMIN` in permitted/effective sets when
`/proc` simultaneously proves a nested PID namespace and a non-initial,
subordinate `uid_map`. That capability is authority inside the disposable user
namespace, not a retained container-namespace capability; every other bit or
process fails the gate. GTK, Qt, and QtWebEngine fixture owners require all four
sets to be zero.

The persistence restart deliberately plants saved-session and autostart canaries
under `/home/xenoteer`, then proves those bytes survive while neither canary is
consumed. This distinguishes safe profile isolation from destructive HOME
cleanup. Run `scripts/container/test-runtime-profiles.sh` for the image-free
materialization/profile/Compose contract before paying the cost of the live
matrix.

The separate Phase 4 live-API gate keeps this already broad Phase 2 matrix from
growing further. It starts one two-CPU container from the same derived image and
requires GTK3, Qt6, Chromium, Firefox ESR, and QtWebEngine. Through authenticated
`xenoteerd` routes it proves direct and ICCCM INCR clipboard reads, inline and
artifact-backed selection ownership, application paste with value-copy restore,
window list/query/resolve/snapshot plus representative xfwm4 operations, and
root/window PNG capture through private artifact range/download/delete. The two
small AT-SPI/GTK helpers emit only content length and digest evidence; expected
clipboard and text bodies are never printed. Every named fixture is mandatory,
so a stale fixture image or unavailable application fails rather than skipping.
Exact application paste is asserted for GTK3, Qt6, Chromium, and Firefox.
QtWebEngine is still mandatory for initial editable-text, window, AT-SPI,
sandbox, and capture checks, but the runner emits an explicit isolated skip for
its empirically reproduced QtWebEngine 6.8.2/PyQt 6.9 X11
forced-accessibility duplicate-paste defect. Its HTML input lacks AT-SPI
EditableText, so semantic insertion is not a
truthful fallback; see `plans/05-keyboard-and-clipboard.md` for the evidence and
re-enable criteria.

The fixture image should normally be rebuilt from the current production image
before this gate runs. For local diagnosis of an intentionally stale cached
fixture layer, `XENOTEERD_BINARY_OVERRIDE` may name an executable current
`xenoteerd` binary; the runner mounts that single binary read-only and still
requires every desktop fixture. CI and release qualification must use a
coherent freshly derived image and leave this override unset.

## Phase 5 AT-SPI live gate

Phase 5 release qualification uses one freshly built desktop-app fixture
container and leaves `XENOTEER_TEST_DAEMON_BINARY` unset. The override exists
only to diagnose a stale local image and is never qualification evidence. The
runner resolves both derived and recorded base tags to immutable image IDs,
rechecks the rootfs layer prefix, and applies hard limits of 2 CPUs, 6 GiB RAM,
512 PIDs, and 4 GiB shared memory. Docker must be rootless or the command must
run as root so the API token bind mount has the same ownership contract as
production startup.

The gate covers GTK3 and Qt6 application restart fencing, protected text
redaction, Chromium and Firefox document-reload reminting, and a 4,096-row
materialized stress surface. The standard GTK3 and Qt6 fixtures cover native
virtualized controls separately; the stress surface is intentionally
materialized so every stable row can be traversed and paginated. A separate
valid depth-24 tree is queried with `max_depth=8` to prove the public
`query_budget_exceeded` boundary.

The pressure cases also cover AT-SPI bus loss/reconnect without a desktop-
generation change, a bounded 5,000-mutation producer with a slow subscriber,
and isolation of an application that exposes a 70,000-byte accessible name
while a healthy GTK sibling remains queryable. Bad-parent and cyclic-topology
handling are bounded model/unit tests; the live fixture's self-relation is not
claimed as relation-hydration coverage. Every live pressure case must leave the
daemon responsive, avoid OOM termination, recover authoritative accessibility
state, and keep token/text canaries out of logs.

Some toolkit accessibility bridges do not re-register after the accessibility
bus is replaced. The reconnect proof therefore rejects the old reference,
relaunches a controlled toolkit client, and requires a fresh AT-SPI-generation
reference while the desktop generation remains unchanged. The event-flood
recovery uses the same controlled-relaunch rule when its producer retained the
dead bridge connection.

GTK3 and Qt6 also exercise the authenticated control exit gate through the
reviewed Phase 3 WebSocket client. One renewable exclusive lease drives semantic
invoke, focus, value, protected set/insert text, and generic `text.insert` with
`strategy=auto` constrained to the semantic policy. Qt supplies reliable
selection readback; GTK exercises the operation but accepts an explicit
unsupported/no-effect or dispatched-unsupported outcome when its bridge cannot
provide reliable post-read evidence. Scroll must either return bounded
before/after geometry when the toolkit exposes the AT-SPI operation or an honest
no-effect unsupported result; the gate does not pretend that every standard
widget is scrollable. A separate physical element click must carry the exact
correlated window birth, at least strong non-conflicting correlation, fresh
geometry, a smooth 250 ms pointer path, and a distinct physical outcome with
`pointer_interpolated=true` and effect stage
`element_physically_clicked`; semantic results are required to retain their own
operation-specific outcomes and effect stages.

Run the coherent gate as follows. The timeout and low-priority wrappers are
optional but recommended on shared development machines:

```sh
sudo scripts/container/build.sh
sudo scripts/container/build-desktop-app-fixture.sh
sudo scripts/container/test-desktop-app-image.sh xenoteer:desktop-apps-test
sudo nice -n 15 ionice -c 3 timeout 25m \
  scripts/container/test-phase5-atspi-live.py xenoteer:desktop-apps-test
```

The coherent no-override Phase 5 qualification passed with production image
`sha256:68508e98bb1f7a0995e96b4b93499cced7247fa7a99f90652c19abec2a52dafb`
and exact derived fixture image
`sha256:1733ddadd8d2235c42ec518bbc06d2053e6eded9d6f4cebd6999708f9470e934`.
The production, desktop matrix, Phase 4 API, and Phase 5 AT-SPI gates all used
the two-CPU policy and left their daemon overrides unset.

Source/package verification should retain the repository's shared heavy-build
lock and two-job limit:

```sh
timeout 5m flock /tmp/codex/xenoteer-heavy-build.lock \
  nice -n 15 ionice -c 3 env CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 \
  cargo test -p xenoteer-atspi --all-features --locked

timeout 10m flock /tmp/codex/xenoteer-heavy-build.lock \
  nice -n 15 ionice -c 3 env CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 \
  cargo test --locked -p xenoteerd \
    application_invalidation_cache_change_precedes_marker_without_duplicate_removals
```
