# Container build and operation

This directory is the Phase 2 container contract: immutable build inputs, a
non-root deterministic XFCE/AT-SPI desktop, an s6-rc v3 dependency graph, a
loopback view-only TigerVNC/noVNC chain, deterministic provenance inventories,
and development/hardening Compose examples. Browsers and toolkit acceptance
applications remain in separate non-distributable test images; they do not
expand the production package or release-inventory boundary.

## Supported build

The initial supported platform is `linux/amd64`. `container/locks/release.lock`
pins the Dockerfile frontend, Debian 13/trixie and Rust OCI indexes by digest,
Debian package repositories to a snapshot timestamp,
noVNC/TigerVNC/websockify artifacts, the CI Dockerfile linter, and both required
s6-overlay archives by upstream SHA-256.
`container/locks/sources.lock` records the corresponding source/evidence URLs.

Build through the lock-aware wrapper:

```sh
scripts/container/validate-locks.sh --online
XENOTEER_IMAGE=xenoteer:dev scripts/container/build.sh
```

The wrapper explicitly enables BuildKit; the pinned Dockerfile frontend and
read-only build-context mounts are required, not optional legacy-builder hints.

The explicit long-idle gate defaults to the Phase 2 acceptance duration of 30
minutes and rechecks readiness, the exact idle process/listener allowlist,
viewer protocol reachability, service PID stability, and orderly shutdown:

```sh
sudo XENOTEER_IDLE_SOAK_SECONDS=1800 \
  scripts/container/test-idle-soak.sh xenoteer:dev
```

Set `XENOTEER_IDLE_SOAK_HARDENED=1` to exercise the read-only-root profile.

The online check downloads and verifies every non-OCI locked source. OCI digest
refreshes must be resolved from the registry and reviewed separately; never paste
a guessed digest. A security refresh advances the base digest and Debian snapshot
together, regenerates package/license evidence, and reruns the full image gates.
The snapshot makes inputs repeatable, not perpetually secure.

The wrapper labels the image with a SHA-256 over Cargo manifests/lockfile,
toolchain declaration, package lists, and container lock files. It also records
the checked-out revision, a deterministic source-tree SHA-256, and whether the
tree was dirty. A dirty development build uses
`<commit>-dirty.<tree-hash-prefix>` as its OCI revision; it is never presented as
the clean commit. An explicit `XENOTEER_REVISION` is accepted only when it equals
the checked-out commit.

The pinned Debian slim image does not guarantee a CA trust directory, so its first APT
transaction uses snapshot.debian.org over HTTP. APT still requires Debian's
signed `InRelease` metadata, verifies package hashes through that signed chain,
and every build stage immediately verifies the downloaded `InRelease` bytes
against the six exact hashes pinned in `release.lock` and `sources.lock`. The
transaction installs `ca-certificates`; all direct s6 downloads then use HTTPS
plus their locked SHA-256. TLS verification is never disabled.

## Production package and viewer boundary

The reviewed package groups are committed separately as `runtime.txt`,
`desktop.txt`, and `viewer.txt`. They are exact direct-request sets installed
with `--no-install-recommends`; broad XFCE metapackages are not used. The image
excludes `dbus-x11`, the Debian `novnc` and `websockify` wrapper packages,
Node.js/network diagnostics, Thunar/Tumbler, display managers, lockers, power
managers, and notification daemons. The final package manifest records every
direct and transitive package with version, architecture, source name/version,
snapshot archive path, and signed-index `.deb` SHA-256.
The build regenerates fontconfig caches and fails unless DejaVu Sans, Liberation
Sans, Noto Sans, Noto CJK, Noto Color Emoji, and Noto Sans Mono resolve to their
expected installed families through `fc-match`.

The only manually extracted production application is Debian noVNC
`1:1.6.0-2`. Its `.deb` is verified before extraction; only
`/usr/share/novnc` and the Debian copyright file cross the build-stage boundary.
An exact sorted file/symlink manifest and critical-entry-point hashes are checked
again by the final filesystem inventory. Xenoteer's `mandatory.json` replaces
the package default and enforces scaling, shared viewing, and `view_only=true`.
`python3-websockify` and `tigervnc-scraping-server` are installed from separately
verified exact `.deb` files. Only control-plane port 8080 is exposed; RFB and
viewer WebSocket listeners stay on loopback inside the container.

## Runtime identity and paths

`/init` remains root so s6 can prepare state and drop privileges. Its tiny
readiness wrappers retain access to root-owned supervision directories; the Xvfb
and `xenoteerd` payload processes run as `xenoteer:xenoteer` (UID/GID 1000).
Arbitrary-UID entrypoints are not supported in release one.

| Path | Required ownership/mode | Operator action |
|---|---|---|
| `/run` | root, writable tmpfs | Required with a read-only root |
| `/run/user/1000` | 1000:1000, 0700 | Created by the runtime oneshot |
| `/tmp` | root, 1777 tmpfs | Required with a read-only root |
| `/dev/shm` | root, 1777, >=4 GiB | Private 4 GiB shm mount used by Compose |
| `/home/xenoteer` | 1000:1000, 0700 | Named volume by default |
| `/workspace` | image: 1000:1000, 0755; mounted storage controls runtime ownership | Named volume by default; prepare bind mounts externally; no boot-time recursive chown |

Startup fails if `/dev/shm` is smaller than 4 GiB or lacks mode 1777. This is an
evidence-driven increase from the earlier 2 GiB proposal: Debian Chromium 150's
launcher injects `--disable-dev-shm-usage` below 4,080,218,931 available bytes
(3.8 GiB). Xenoteer rejects that fallback, uses a private sparse 4 GiB tmpfs,
and never shares host IPC. Shm pages consume the container's memory-cgroup
budget; the hardened 6 GiB limit does not guarantee 4 GiB of shm plus 2 GiB of
browser/process memory, and workloads must be sized from observed peak usage.

The production display contract is exactly 1920x1080, 24-bit color, at 96 DPI.
`XVFB_SCREEN_WIDTH`, `XVFB_SCREEN_HEIGHT`, and `XVFB_SCREEN_DEPTH` remain visible
in the OCI environment only as explicit contract values; overriding any of them
to a different value fails startup. The X11 readiness probe independently checks
the live pixel geometry, root depth, 96x96 DPI report, and XTEST extension.

The runtime oneshot accepts daemon configuration only under the strict
`XENOTEER__SECTION__FIELD` grammar used by the Rust loader. Single-underscore,
extra-nesting, empty-segment, lowercase, and otherwise malformed Xenoteer names
fail closed without printing their names or values. Every syntactically valid key
is passed through to the daemon, including unknown or empty values, so its strict
typed decoder—not an s6 allowlist—decides whether the setting is supported. The
s6 environment files use value-plus-terminator encoding and `s6-envdir -f -L` so
an empty value remains set and embedded/trailing newlines are preserved. Secret
contents remain file-mounted and must never enter this environment path.

## Service graph and readiness

The Phase 2 s6-rc graph is:

```text
runtime-directories -> desktop-profile --+
                  \-> machine-id --------+-> xvfb -> session-dbus -> atspi -> xfce -> xenoteerd
                  \-> xauthority --------+                                  \-> x0tigervnc -> websockify
```

Xvfb is ready only after an authenticated `xdpyinfo` round trip proves the fixed
geometry, depth, 96 DPI, and XTEST extension. The daemon is ready to s6 only after
`/livez` responds. Desktop readiness additionally proves the single session bus,
AT-SPI registry, XFCE session/window manager, and live X11 desktop capability.
The optional view-only chain starts only after XFCE and binds both RFB and viewer
WebSocket listeners to loopback. Any critical daemon, Xvfb, or XFCE exit asks s6
PID 1 to halt the container. Xvfb uses `-nolisten tcp` and an
MIT-MAGIC-COOKIE-1 authority file; `-ac` is prohibited.

Before Xvfb starts, `desktop-profile` atomically materializes immutable profile
assets into `/run/user/1000/xdg`. It accepts only `bare` or `standard`, never
reads saved XFCE state or autostarts from persistent HOME, and never deletes
persistent HOME content. `bare` runs `xfwm4 --compositor=off`, `xfsettingsd`, and
`xfdesktop`; `standard` adds the panel. Neither starts the Thunar daemon. Both
disable SaveOnExit, compatibility/agent launches, blanking, power management,
locking, notifications, compositor effects, and session restoration, while
fixing one workspace, click-to-focus, theme, fonts, DPI, layout, and locale.

## Development run

Create a token file containing at least 256 random bits. A file-backed Compose
secret is a bind mount: Docker Compose does not implement the service-level
`uid`, `gid`, or `mode` settings for this source type, so Xenoteer does not
declare them. Before startup, the host file must map to UID 1000 inside the
container and have mode 0400 or 0600. With a rootful daemon this normally means
host UID 1000; prepare and verify it explicitly:

```sh
install -m 0600 -o 1000 -g 1000 /dev/null /absolute/path/to/xenoteer-token
openssl rand -out /absolute/path/to/xenoteer-token 32
stat -c '%u:%g %a' /absolute/path/to/xenoteer-token
export XENOTEER_TOKEN_FILE=/absolute/path/to/xenoteer-token
XENOTEER_IMAGE=xenoteer:dev scripts/container/build.sh
docker compose -f compose.dev.yml --profile dev up --no-build
```

Only `127.0.0.1:8080` is published. X11, D-Bus, and RFB are never published.
The Compose files deliberately contain no `build:` stanza: they consume the
image produced by the lock-aware wrapper, and `--no-build` prevents a direct
Compose invocation from bypassing verified dependency/source labels.
Startup independently requires the mounted token to be owned by container UID
1000 with mode 0400 or 0600 and fails closed otherwise. Rootless Docker and
user-namespace remapping translate host IDs; do not assume host UID 1000 maps to
container UID 1000. Verify the mapping for that daemon (for example with a
one-off container mounting the same file) or use a deployment-specific secret
provisioner that produces the required in-container metadata. The token contents
must not be put in environment variables, build arguments, URLs, or command-line
arguments. Set `DESKTOP_PROFILE=standard` only when the deterministic panel is
required; `bare` is the default and smallest automation surface.

## Hardened candidate

```sh
docker compose \
  -f compose.dev.yml \
  -f compose.hardened.yml \
  --profile hardened up --no-build
```

The overlay makes the root read-only, supplies `/run` and `/tmp` tmpfs mounts,
drops all capabilities before adding exactly seven: five for root initialization
and UID transition (`CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `SETGID`, `SETUID`),
`KILL` so root supervision can terminate UID-1000 payloads, and `SYS_CHROOT` for
Chromium's layer-1 sandbox. It enables no-new-privileges and
applies process/file/memory/CPU
and log limits. `/run` deliberately remains executable because s6-overlay runs
its stage-2 process there.

`CAP_KILL` is a supervision capability, not an application capability. The
image gate boots the otherwise exact hardened profile with only `CAP_KILL`
removed, proves Xvfb and xenoteerd are UID 1000, and then proves PID 1 cannot
stop them before a short Docker deadline: the container is forcibly killed with
exit 137. The complete seven-cap profile stops the same payloads cleanly with
exit 0.

Critical finish hooks query s6-supervise's live `wantedup` state from their
guaranteed service-directory working directory. A requested operator/s6-rc stop
sets `wantedup=false` before signalling the process, including while startup is
still in progress, and therefore preserves graceful exit 0. An unsolicited
critical death remains `wantedup=true`; its finish hook atomically records a
nonzero child/signal result for s6-overlay, requests halt, and exits 125 so the
service cannot respawn. Failure to read supervision intent fails closed. The
built-image gate covers immediate startup stop, ready normal/hardened stops, and
unexpected Xvfb/xenoteerd exits in both profiles.

This is a candidate, not a blanket compatibility claim:

- prove s6 ownership setup and clean shutdown on the deployed Docker version;
- prove Chromium's user-namespace and seccomp sandbox remains active;
- scan listeners from inside and outside the container;
- boot twice with both clean and persistent home volumes;
- kill Xvfb and `xenoteerd` separately and verify truthful failure/no-respawn;
- run the full browser and QtWebEngine shared-memory stress fixtures.

Never troubleshoot by adding `--privileged`, mounting the Docker socket or host X
socket, using host PID/network/IPC, adding `-ac`, or launching a browser with
`--no-sandbox`.

Phase 2 acceptance requires the production, viewer-denial,
clean/persistent-profile, toolkit/browser, hardened sandbox, and idle-soak gates
below to pass against one settled image digest. The browser policy is
deliberately coupled to the pinned Docker Engine 29.1.3 Moby seccomp baseline;
an engine, kernel, package, capability, or sandbox-policy change reopens the
relevant proof rather than inheriting this result by assumption.

## Verification

Docker-independent checks:

```sh
scripts/container/test-static.sh
scripts/container/test-runtime-profiles.sh
```

Docker-required gates:

```sh
sudo --preserve-env=XENOTEER_IMAGE scripts/container/build.sh
sudo docker inspect xenoteer:dev
sudo scripts/container/test-image.sh xenoteer:dev
sudo scripts/container/test-browser-spike.sh xenoteer:dev xenoteer:browser-spike
sudo env XENOTEER_NOVNC_SPIKE_BASE_IMAGE=xenoteer:dev \
  scripts/container/test-novnc-spike.sh
sudo scripts/container/build-desktop-app-fixture.sh
sudo scripts/container/test-desktop-app-image.sh xenoteer:desktop-apps-test
sudo XENOTEER_IDLE_SOAK_SECONDS=1800 \
  scripts/container/test-idle-soak.sh xenoteer:dev
```

The image test waits for healthy status, confirms PID 1 and UID 1000 payloads,
performs authenticated and unauthenticated X11 tests, scans for the forbidden X11
TCP port, verifies manifests and endpoint semantics, exercises bounded SIGTERM,
kills each critical longrun, proves production RFB/WebSocket input and clipboard
denial, and proves a missing secret fails closed. The
desktop-app fixture image adds pinned GTK3, Qt6, Chromium, Firefox ESR,
QtWebEngine, AT-SPI, and window-inspection packages without expanding the
production boundary. Its gate exercises bare and standard process sets,
ephemeral-profile rematerialization across a persistent-HOME restart, GTK/Qt and
browser accessibility trees, sandboxed browser subprocesses, listener exposure,
and the full application matrix under the read-only-root hardened profile.
Separate spike images retain a redundant end-to-end noVNC/RFB proof. Their exact assertions,
measured results, and revisit triggers are recorded in
[`spikes/browser/README.md`](spikes/browser/README.md) and
[`spikes/novnc/README.md`](spikes/novnc/README.md). These acceptance/spike images
remain non-distributable and outside the production image's final inventory.

## License and source evidence

`scripts/licenses/inventory-first-party.sh` rejects repository files that have no
explicit path-to-license rule and emits a deterministic manifest.
`inventory-debian.sh` fails any installed package with missing source-package,
archive checksum, or copyright provenance. `generate-cargo-manifest.sh` derives the production
binary's normal/build transitive crate closure from `Cargo.lock` and emits SPDX
license evidence without admitting dev-only crates. `generate-s6-manifest.sh`
records every extracted s6 regular file and symlink against the locked archives
and preserves the upstream ISC license.

Before APT's verified snapshot metadata is removed,
`generate-debian-installed-manifest.sh` records every regular file and symlink
in the post-install filesystem with content/target identity, mode, UID, GID,
dpkg owner, and verification class. Present package payloads covered by dpkg
MD5 data must match it, and every owner must resolve to the all-installed-package
manifest backed by signed snapshot indexes and exact `.deb` hashes. The final
inventory requires every baseline entry to remain present and byte/type/metadata
identical; a path that merely remains named in a package `.list` cannot inherit
Debian provenance after being overwritten.

Finally, `inventory-final-image.sh` classifies every regular file and symlink in
the production root filesystem as first-party, Cargo, s6, locked extracted
noVNC, Debian-owned, narrowly generated runtime content, or an explicit reviewed
exception. Unknown paths, missing package/source/copyright data, stale Cargo
closure, or an unmatched s6/noVNC artifact fail the build. Directories and device nodes are outside this file-level
inventory; the narrow volatile/generated categories and exceptions are reviewed
in `container/licenses/final-image-exceptions.tsv`. Glob exceptions cannot admit
paths absent from the exact installed baseline, symlink classes require an
observed symlink, and all first-party/s6/noVNC/baseline manifest entries have
reverse completeness checks. Each row records content or
symlink-target identity plus mode, UID, and GID. The inventory accounts for its
own output with an explicit `SELF-REFERENTIAL` hash marker because embedding its
own cryptographic digest would be recursive. The image embeds the root
`NOTICE`, both dependency/source locks, and all generated evidence under
`/usr/share/doc/xenoteer/`.

These gates provide a complete classified production filesystem, not a
claim that release publishing is finished. A public release still requires the
corresponding-source bundle, consolidated third-party notices/SBOM, offline
verification, provenance attestation, vulnerability-policy decision, and
signature specified in the licensing and release plans. Browser/toolkit fixture
images remain explicitly test-only and non-distributable.
