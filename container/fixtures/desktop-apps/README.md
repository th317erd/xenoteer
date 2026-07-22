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
```

Both builds use the Debian snapshot recorded in `container/locks/release.lock`.
Every direct acceptance package is version-pinned in `packages.lock`; apt still
resolves their dependency closure from that immutable snapshot. Electron is not
available as a Debian trixie runtime package, so `artifacts.lock` records the
official stable v43.1.1 Linux x64 release URL and SHA-256. The build verifies the
official checksum before extraction and retains Electron's `LICENSE` and
`LICENSES.chromium.html`; no Electron bytes enter the production image.

The build wrapper resolves the production tag to an immutable `sha256:` image
ID before invoking Docker, uses that ID in `FROM`, records it in the derived
image label, then compares the exact rootfs-layer prefix. The live matrix again
resolves the derived tag once, uses only its immutable ID, checks the recorded
base is locally present, and repeats the ancestry proof. The test image inherits
the production entrypoint, desktop graph, profiles, and seccomp contract.

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

The fixture image should normally be rebuilt from the current production image
before this gate runs. For local diagnosis of an intentionally stale cached
fixture layer, `XENOTEERD_BINARY_OVERRIDE` may name an executable current
`xenoteerd` binary; the runner mounts that single binary read-only and still
requires every desktop fixture. CI and release qualification must use a
coherent freshly derived image and leave this override unset.
