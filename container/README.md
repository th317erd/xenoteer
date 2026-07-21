# Container build and operation

This directory is the Phase 0 container contract: immutable build inputs, a
non-root X11 runtime, an s6-rc v3 dependency graph, deterministic provenance
inventories, and development/hardening Compose examples. The production image
remains intentionally small: XFCE, browsers, AT-SPI, and the viewer are not yet
installed in that image. Separate CI-only derived images have completed the
browser-sandbox and noVNC/TigerVNC feasibility proofs without weakening the
production image's release inventory boundary.

## Supported build

The initial supported platform is `linux/amd64`. `container/locks/release.lock`
pins the Debian and Rust OCI indexes by digest, Debian package repositories to a
snapshot timestamp, and both required s6-overlay archives by upstream SHA-256.
`container/locks/sources.lock` records the corresponding source/evidence URLs.

Build through the lock-aware wrapper:

```sh
scripts/container/validate-locks.sh --online
XENOTEER_IMAGE=xenoteer:dev scripts/container/build.sh
```

The online check downloads and verifies every non-OCI locked source. OCI digest
refreshes must be resolved from the registry and reviewed separately; never paste
a guessed digest. A security refresh advances the base digest and Debian snapshot
together, regenerates package/license evidence, and reruns the full image gates.
The snapshot makes inputs repeatable, not perpetually secure.

Debian stable-slim does not guarantee a CA trust directory, so its first APT
transaction uses snapshot.debian.org over HTTP. APT still requires Debian's
signed `InRelease` metadata, verifies package hashes through that signed chain,
and the exact metadata hashes are independently pinned in `sources.lock`. The
transaction installs `ca-certificates`; all direct s6 downloads then use HTTPS
plus their locked SHA-256. TLS verification is never disabled.

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
| `/workspace` | deployment-defined | Named volume; no boot-time recursive chown |

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

The Phase 0 s6-rc graph is:

```text
runtime-directories -> machine-id ----+
                  \-> xauthority -----+-> xvfb -> xenoteerd
```

Xvfb is ready only after an authenticated `xdpyinfo` round trip proves the fixed
geometry, depth, 96 DPI, and XTEST extension. The daemon is ready to s6 only after
`/livez` responds. During Phase 0 the OCI health check also uses `/livez` because
the desktop capability probes are not wired into the daemon yet. `/readyz`
truthfully remains 503; Phase 2 moves the OCI check to `/readyz` only after those
probes exist. Any daemon or Xvfb exit is critical and asks s6 PID 1 to halt the
container. Xvfb uses `-nolisten tcp` and an MIT-MAGIC-COOKIE-1 authority file;
`-ac` is prohibited.

The next desktop phase extends this graph with one session D-Bus, AT-SPI, XFCE,
and optional loopback viewer services. It must keep the same dependency names and
readiness semantics.

## Development run

Create a token file containing at least 256 random bits and make it owner-readable
only. Then run:

```sh
export XENOTEER_TOKEN_FILE=/absolute/path/to/xenoteer-token
docker compose -f compose.dev.yml --profile dev up --build
```

Only `127.0.0.1:8080` is published. X11, D-Bus, and RFB are never published.
The token file is mounted as a Compose secret readable by UID 1000; its contents
must not be put in environment variables, build arguments, URLs, or command-line
arguments.

## Hardened candidate

```sh
docker compose \
  -f compose.dev.yml \
  -f compose.hardened.yml \
  --profile hardened up
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

The candidate has passed the Phase-0 runtime and browser profiles on the pinned
Docker Engine 29.1.3 environment. The browser policy is deliberately coupled to
that engine's exact Moby seccomp baseline; an engine, kernel, package, capability,
or sandbox-policy change reopens the relevant proof rather than inheriting this
result by assumption.

## Verification

Docker-independent checks:

```sh
scripts/container/test-static.sh
```

Docker-required gates:

```sh
sudo --preserve-env=XENOTEER_IMAGE scripts/container/build.sh
sudo docker inspect xenoteer:dev
sudo scripts/container/test-image.sh xenoteer:dev
sudo scripts/container/test-browser-spike.sh xenoteer:dev xenoteer:browser-spike
sudo env XENOTEER_NOVNC_SPIKE_BASE_IMAGE=xenoteer:dev \
  scripts/container/test-novnc-spike.sh
```

The image test waits for healthy status, confirms PID 1 and UID 1000 payloads,
performs authenticated and unauthenticated X11 tests, scans for the forbidden X11
TCP port, verifies manifests and endpoint semantics, exercises bounded SIGTERM,
kills each critical longrun, and proves a missing secret fails closed. The two
derived-image gates then exercise Chromium and QtWebEngine under both normal and
hardened profiles and drive the real noVNC/RFB chain. Their exact assertions,
measured results, and revisit triggers are recorded in
[`spikes/browser/README.md`](spikes/browser/README.md) and
[`spikes/novnc/README.md`](spikes/novnc/README.md). Both derived images carry a
non-distributable label because their packages are intentionally outside the
production image's final inventory.

## License and source evidence

`scripts/licenses/inventory-first-party.sh` rejects repository files that have no
explicit path-to-license rule and emits a deterministic manifest.
`inventory-debian.sh` fails any installed package with missing source-package or
copyright provenance. `generate-cargo-manifest.sh` derives the production
binary's normal/build transitive crate closure from `Cargo.lock` and emits SPDX
license evidence without admitting dev-only crates. `generate-s6-manifest.sh`
records every extracted s6 regular file and symlink against the locked archives
and preserves the upstream ISC license.

Finally, `inventory-final-image.sh` classifies every regular file and symlink in
the production root filesystem as first-party, Cargo, s6, Debian-owned, narrowly
generated runtime content, or an explicit reviewed exception. Unknown paths,
missing package/source/copyright data, stale Cargo closure, or an unmatched s6
artifact fail the build. Directories and device nodes are outside this file-level
inventory; the narrow volatile/generated categories and exceptions are reviewed
in `container/licenses/final-image-exceptions.tsv`. The image embeds the root
`NOTICE`, source locks, and all generated evidence under
`/usr/share/doc/xenoteer/`.

These gates provide a complete classified Phase-0 production filesystem, not a
claim that release publishing is finished. A public release still requires the
corresponding-source bundle, consolidated third-party notices/SBOM, offline
verification, provenance attestation, vulnerability-policy decision, and
signature specified in the licensing and release plans. The CI-only browser and
viewer derived images must first be brought inside the same production inventory
and release-evidence boundary.
