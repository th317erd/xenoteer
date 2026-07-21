# Browser sandbox Phase-0 spike

This directory is a CI-only feasibility proof. The derived image is deliberately
labelled `com.aeor.xenoteer.distributable=false`: it adds packages and fixture
files after the production image's complete final-file inventory. It must not be
published until the browser layer is brought inside the production inventory,
SBOM, notice, and vulnerability-policy gates.

## Locked inputs

The image inherits Xenoteer's Debian snapshot `20260719T000000Z` and pins every
direct browser-spike input exactly:

| Input | Debian version |
| --- | --- |
| `chromium` | `150.0.7871.124-1~deb13u1` |
| `chromium-sandbox` | `150.0.7871.124-1~deb13u1` |
| `python3-pyqt6.qtwebengine` | `6.9.0-1` |
| `python3-websocket` | `1.8.0-2` |

Transitive Debian packages are resolved only from that immutable snapshot. The
derived image writes `browser-spike-package-manifest.tsv`, including each binary
and source package version plus the hash of its Debian copyright evidence.

## Decision and proof boundary

The retained Phase-0 choice is Debian Chromium plus PyQt6 QtWebEngine on X11.
Both execute as the unprivileged desktop identity, UID/GID 1000, through the
runtime's Xauthority-authenticated Xvfb display. The executable proof is
[`scripts/container/test-browser-spike.sh`](../../../scripts/container/test-browser-spike.sh).
It starts each browser twice: once with the normal container profile and once
with the hardened read-only profile.

The Chromium test renders a local deterministic page, inspects its DOM, captures
a PNG through DevTools, and then checks `chrome://sandbox` for namespace,
seccomp-BPF, and sandboxed-process success. It independently inspects every
renderer under `/proc`: the owner must be UID 1000 and `Seccomp` must be mode 2.

The QtWebEngine test renders the same page in a visible `QWebEngineView`, checks
its DOM through Qt and independently captures a PNG from that Qt renderer over a
loopback-only DevTools endpoint. It audits each Qt renderer under `/proc`; each
must be UID 1000 with `Seccomp: 2` and `NoNewPrivs: 1`.

Neither path uses `--no-sandbox`, `QTWEBENGINE_DISABLE_SANDBOX`,
`--disable-dev-shm-usage`, `--privileged`, host X11 sockets, or host IPC. The
fixture fails if a forbidden browser flag reaches a renderer. The only Qt
Chromium flags are `--disable-gpu` and
`--remote-debugging-address=127.0.0.1`; the first reflects the absence of an
exposed GPU and the second confines the temporary proof endpoint. QtWebEngine
itself records `--disable-setuid-sandbox` because Qt uses its user-namespace and
seccomp layers rather than Chromium's setuid helper; the `/proc` proof remains
mandatory. Chromium also disables nondeterministic background services and GPU
use, but does not weaken its sandbox.

The checked-in seccomp policy is Docker-version coupled. Its baseline is the
Moby profile vendored by Docker Engine tag `docker-v29.1.3`, commit
`fbf3ed25f893e6ce21336f1101590e40a13934f4`; the locked raw JSON has SHA-256
`01536f1d1df938ae611eba20d6349e0de7a99b6ecdee1549427a0b01b8301e28`.
`docker-default-seccomp.json` is a `jq`-canonicalized copy, so its byte hash
(`f17cb7cf3c40ab6a42d978a3eea027062f18ee72d2ba5edc3a5cbdf58c67ab58`)
intentionally differs while its JSON is
required to be semantically identical. `seccomp_profile.json` is exactly that
baseline plus the official Playwright unconditional allowance for only `clone`,
`setns`, and `unshare`. Static tests strip that one rule and compare canonical
JSON; the online source-lock gate verifies both the Moby raw artifact and pinned
Playwright rule evidence. Any Docker Engine change requires rebasing and rerunning
the syscall and browser proofs.

Both JSON policies are Apache-2.0 derivatives rather than Xenoteer BSL works.
The repository retains byte-exact `LICENSE` and `NOTICE` files from the pinned
Moby and Playwright revisions under `licenses/`, locks their upstream URLs and
SHA-256 values in `container/locks/sources.lock`, and tests their exact source
inventory classification. The root `NOTICE` records which upstream contributes
each portion of the combined profile.

### Verified profiles

The following table is filled only from an actual invocation of the executable
spike, not from package presence:

| Profile | Container restrictions | Chromium | QtWebEngine |
| --- | --- | --- | --- |
| Normal | nonroot desktop, private Xvfb, 4 GiB `/dev/shm`, pinned Moby 29.1.3 seccomp baseline plus user-namespace syscalls | PASS: DOM + 640x393 PNG; PID/network namespaces and seccomp-BPF reported Yes; renderer UID 1000/Seccomp 2 | PASS: DOM + 640x480 PNG; subprocess tree UID 1000, nested PID namespaces, Seccomp 2, NNP 1 |
| Hardened | read-only rootfs, seven-cap allowlist, `no-new-privileges`, PID/CPU/6 GiB aggregate memory limits, private tmpfs/volumes | PASS with the same `chrome://sandbox` and `/proc` assertions | PASS with the same DOM/PNG and full subprocess-tree assertions |

## Runtime contract

- `/dev/shm` is a private 4 GiB Docker shm mount. Debian Chromium 150's launcher
  injects the rejected `--disable-dev-shm-usage` flag whenever available shm is
  below 4,080,218,931 bytes (3.8 GiB), so the earlier 2 GiB proposal failed the
  executable flag audit. Smaller/default Docker shm is rejected before launch.
- `/run` is a private 64 MiB `rw,nosuid,nodev,exec` tmpfs. `exec` is required by
  s6-overlay's stage-0/stage-2 handoff; this was verified independently.
- `/tmp` is a private 1 GiB `rw,nosuid,nodev,noexec` tmpfs.
- `/home/xenoteer` and `/workspace` are anonymous writable volumes in the
  hardened profile. The API token is the only bind mount and is read-only.
- The hardened profile drops all capabilities, then restores `CHOWN`,
  `DAC_OVERRIDE`, `FOWNER`, `SETGID`, and `SETUID` for PID 1 to construct and
  enter the UID-1000 runtime. `KILL` lets root supervision terminate those
  UID-1000 payloads during critical failure and clean shutdown. `SYS_CHROOT` is
  the seventh, narrowly proven
  capability consumed by Chromium's layer-1 namespace/chroot sandbox: the five
  init capabilities failed before the zygote hello, while adding only
  `SYS_CHROOT` passed `chrome://sandbox`. Browser and renderer processes run as
  UID/GID 1000.
- No browser port is published. DevTools binds only to `127.0.0.1` inside the
  container and uses an ephemeral port.

The normal and hardened checks are intentionally separate. `no-new-privileges`
and host user-namespace/AppArmor policy can change which Chromium sandbox layer
is available; a host whose kernel disables unprivileged user namespaces may fail
this spike. That is a deployment incompatibility, not permission to add
`--no-sandbox`. Operators should confirm the host permits the browser's namespace
sandbox and keep the default Docker seccomp policy enabled.

## Rejected alternatives

- Disabling the Chromium/Qt sandbox was rejected: it converts a renderer exploit
  into direct access to the desktop session and automation token.
- Sharing host `/dev/shm`, host IPC, or `/tmp/.X11-unix` was rejected because it
  weakens tenant isolation and reintroduces host display trust.
- `--disable-dev-shm-usage` was rejected; disk-backed `/tmp` is not an equivalent
  isolation or performance contract.
- Running either browser as root was rejected. The test treats UID 1000 as an
  invariant and verifies renderer ownership rather than trusting the launcher.
- Bundling a downloaded browser tarball was rejected for Phase 0. Snapshot-pinned
  Debian packages provide source-package provenance, copyright evidence, and a
  coherent security-update channel.

## Revisit triggers

Repeat both profiles and re-record this decision whenever the Debian snapshot,
Chromium, Qt/PyQt6, Docker default seccomp policy, base kernel/user-namespace
policy, s6-overlay, X server, capability set, shm size, or rootfs/tmpfs layout
changes. Promotion from a spike to a distributable browser image additionally
requires a refreshed complete final-file inventory, Cargo/Debian/third-party
SBOM aggregation, notices, CVE policy, and multi-architecture proof.

Upstream behavior references:

- [Chromium Linux sandbox design](https://chromium.googlesource.com/chromium/src/+/refs/tags/140.0.7339.129/sandbox/linux/README.md)
- [Qt WebEngine platform notes](https://doc.qt.io/qt-6/qtwebengine-platform-notes.html)
