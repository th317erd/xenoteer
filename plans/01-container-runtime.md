# Container runtime and service supervision

## 1. Scope and outcome

This plan defines how the image is assembled and how its process tree starts,
becomes ready, degrades, and shuts down. The outcome is one unprivileged Docker
container with a real PID 1, a non-root desktop session, deterministic service
ordering, and truthful liveness/readiness endpoints.

It does not define fleet orchestration, host networking policy, or application
automation behavior.

## 2. Runtime topology

```text
/init (s6-overlay, PID 1, starts as root)
  |
  +-- runtime-directories (oneshot)
  +-- machine-id (oneshot)
  +-- Xauthority (oneshot)
  +-- system D-Bus (longrun, only if package services require it)
  +-- Xvfb (longrun, readiness probe)
  +-- session D-Bus (longrun, desktop uid, readiness probe)
  +-- AT-SPI bus/registry (longrun, desktop uid)
  +-- XFCE session (longrun, desktop uid)
  +-- xenoteerd (longrun, desktop uid, critical)
  +-- x11vnc (longrun, desktop uid, loopback/view-only)
  +-- websockify/noVNC static server (longrun, desktop uid, loopback)
```

Use s6-rc v3 service definitions, not the legacy `/etc/services.d` layout.
Dependencies describe boot order; readiness notifications describe usability.
Service names and their dependency edges are an integration API and are tested.

## 3. Base-image policy

### 3.1 Selection

- Build from Debian stable `slim` for the selected architecture.
- Release builds pin `FROM` to an immutable digest while retaining the human
  tag in a comment/metadata label.
- Enable Debian `main` only by default. Adding `contrib`, `non-free`, or
  `non-free-firmware` requires a dependency/license decision.
- Install with `--no-install-recommends`; list every deliberately required
  runtime package explicitly.
- Do not run `apt upgrade` opportunistically in the Dockerfile. Rebuild against
  a deliberately advanced base digest and package snapshot.

### 3.2 Reproducibility versus security updates

Two inputs are maintained:

1. `release.lock` identifies the base digest, s6 artifacts and checksums, Rust
   toolchain, Cargo lock, noVNC/websockify sources, and Debian snapshot time.
2. `security-candidate.lock` is advanced by scheduled automation to test current
   security packages before it replaces the release lock.

Debian snapshot repositories make an old build reproducible, but intentionally
freeze vulnerabilities too. Never call a snapshot "secure" merely because it is
reproducible. The release process rebuilds regularly, reports the snapshot date,
and fails on an expired vulnerability waiver.

### 3.3 Build hygiene

- Combine package index update and installation in one build step; delete apt
  lists in that same layer.
- Verify downloaded s6/noVNC/websockify assets with pinned SHA-256 and, where
  upstream provides it, signature verification.
- Use BuildKit secret mounts for credentials; secrets must not become layers,
  build arguments, labels, or provenance fields.
- Compile Rust in a builder stage. Copy only the stripped binaries, required
  dynamic libraries, license data, and runtime assets into the final image.
- Record OCI labels for source revision, version, creation time, license,
  documentation URL, base digest, and protocol version.

## 4. Users, ownership, and runtime directories

### 4.1 Identity model

- PID 1 starts as root only to prepare runtime paths and switch identities.
- All desktop-facing longruns execute as a fixed `xenoteer` user and group,
  initially UID/GID 1000 inside the image.
- Applications launched by the API run as the same desktop user in release one;
  per-application UIDs are future hardening.
- `xenoteerd` is not root and has no Docker socket, host devices, or ambient
  capabilities.

The fixed image UID makes desktop ownership deterministic. Host-mounted
workspace ownership is the deployer's responsibility. The deprecated s6
`fix-attrs` mechanism is not used to recursively chown mounts at boot.

### 4.2 Required writable paths

| Path | Owner/mode | Purpose | Read-only-root deployment |
|---|---|---|---|
| `/run` | root:root 0755 tmpfs | s6 state and runtime hierarchy | writable tmpfs required |
| `/run/user/1000` | xenoteer:xenoteer 0700 | XDG runtime, session bus | created by oneshot |
| `/tmp` | root:root 1777 tmpfs | X11 socket and application temp | writable tmpfs required |
| `/dev/shm` | root:root 1777 tmpfs | browser/toolkit/MIT-SHM | 1 GiB min, 2 GiB recommended |
| `/home/xenoteer` | xenoteer:xenoteer 0700 | profiles/config/cache | volume or writable layer |
| `/workspace` | deployment-defined | bot-visible files | optional explicit mount |

Set `XDG_RUNTIME_DIR=/run/user/1000`. Do not point it at `/tmp`; D-Bus and
desktop services expect a private, correctly owned directory.

For `--read-only`, set `S6_READ_ONLY_ROOT=1` and mount the writable locations
above. s6 assumes `/run` is writable and `/var/run` resolves to `/run`.

### 4.3 Arbitrary UID/rootless caveat

s6-overlay needs a correctly owned `/run` early in startup. OpenShift-style
random UIDs cannot repair root-owned `/run` without privilege. Arbitrary-UID
support is not claimed in release one. A future profile must have the runtime
pre-mount `/run` owned by the selected UID and prove all desktop package paths
work without root preinit.

Docker's rootless daemon is supported as a host configuration, subject to its
documented cgroup/network limitations. This is distinct from forcing the image
entrypoint to an arbitrary non-root UID.

## 5. s6-overlay configuration

### 5.1 Image settings

Set and test:

- `ENTRYPOINT ["/init"]`
- `S6_BEHAVIOUR_IF_STAGE2_FAILS=2` so initialization failure terminates the
  container rather than continuing in an unknowable state.
- `S6_CMD_WAIT_FOR_SERVICES=1` only if a CMD is retained; preferably represent
  all runtime components as s6-rc services and use no long-lived CMD.
- `S6_SERVICES_READYTIME=5000` only as a ceiling for non-notifying legacy
  services; critical services get explicit probes/notifications.
- `S6_KILL_GRACETIME=10000` and `S6_SERVICES_GRACETIME=15000` initially, with
  container `--stop-timeout` greater than the combined shutdown budget.
- `S6_KEEP_ENV=0`. Each service receives only an explicit environment directory
  plus required container configuration.

Do not log to an internal persistent directory by default. Services write
structured logs to stdout/stderr for the container runtime. Optional rotated
file logging requires an explicit volume and retention policy.

### 5.2 Service definitions

Every longrun has:

- `type=longrun`;
- an executable `run` script ending in `exec`;
- dependency files;
- `notification-fd` plus native notification, or `s6-notifyoncheck` with a
  bounded probe;
- a finish policy that distinguishes expected shutdown from crashes;
- stdout/stderr capture without leaking secret environment values;
- execution through `s6-setuidgid xenoteer` for desktop services.

Use oneshots for filesystem initialization and deterministic configuration.
Oneshots must be idempotent: a container restart over a persistent home must not
append duplicate settings or corrupt profiles.

### 5.3 Dependency graph

Required ordering:

```text
base
  -> runtime-directories
  -> machine-id
  -> xauthority
  -> xvfb
       -> session-dbus
            -> atspi
            -> xfce
                 -> xenoteerd
                 -> x11vnc
                      -> websockify
```

`xenoteerd` may start after Xvfb and buses, but does not signal its own readiness
until the window manager and AT-SPI probe have resolved. Viewing may be marked
degraded without making input unavailable; lack of Xvfb or daemon is fatal.

## 6. X11 and D-Bus bootstrap

### 6.1 Xauthority

- Allocate a display number from configuration, default `:99`.
- Create an MIT-MAGIC-COOKIE-1 credential in a root-prepared file owned 0600 by
  the desktop user.
- Start Xvfb with `-auth`, `-nolisten tcp`, fixed screen/depth/DPI, and no `-ac`.
- Export identical `DISPLAY` and `XAUTHORITY` into every desktop service.
- Verify access with an authenticated X11 round trip, not only socket existence.

`-ac` is permitted only in an isolated test fixture specifically proving auth
failure behavior; it is never a production option.

### 6.2 D-Bus

Use Unix-domain buses only:

- System bus, if needed: standard `/run/dbus/system_bus_socket`, constrained by
  packaged policies and not published.
- Session bus: `unix:path=/run/user/1000/bus`, created once and shared by XFCE,
  AT-SPI, applications, and daemon.
- Accessibility bus: acquired through the AT-SPI bus launcher/registry and
  inherited/discovered by accessible applications.

Do not use D-Bus TCP transports. Do not let individual applications
`dbus-launch` private competing session buses. The bootstrap service creates
the single session address, records it in the service environment, and probes
`org.freedesktop.DBus` before declaring readiness.

Create a stable `/etc/machine-id` at image build or first boot as appropriate
for D-Bus, but do not share the host machine ID. If generated per container,
persisting home state must not assume it remains stable across new containers.

## 7. Health model

### 7.1 States

`DesktopSupervisor` reports:

- `starting`: dependency graph is still converging;
- `ready`: all required capabilities passed probes;
- `degraded`: core control works but an optional capability is unavailable;
- `not_ready`: a required capability failed or restarted;
- `stopping`: shutdown is in progress.

Capabilities are reported individually: `x11`, `window_manager`, `session_bus`,
`atspi`, `input`, `capture`, `viewer`, `clipboard`, `launch`.

### 7.2 Endpoints and probes

- `/livez`: daemon event loop responds; no deep dependencies. Suitable for
  determining process liveness, not traffic admission.
- `/readyz`: required capabilities are ready and supervisor generation matches
  the current X server/session. Returns non-2xx otherwise.
- `/v1/status`: authenticated detailed state, versions, extension availability,
  current generation, queue depth, and redacted effective configuration.

Readiness probe sequence:

1. X socket accepts authenticated connection.
2. XTEST extension version is present.
3. root geometry/depth matches configured profile.
4. XFCE window manager advertises EWMH support.
5. session bus responds.
6. AT-SPI registry is reachable, or capability is explicitly configured
   optional.
7. input actor performs a harmless QueryPointer barrier.
8. capture actor reads a small root rectangle.
9. API listener is bound.

Viewer readiness is optional unless `viewer.required=true`.

### 7.3 Restart policy

- Xvfb death invalidates the entire desktop generation. Stop the container in
  release one; do not attempt to attach existing applications to a new X server.
- Session bus or XFCE death makes the desktop not ready. One bounded restart may
  be attempted only if it necessarily restarts the entire desktop subtree and
  increments generation. Prefer container restart for release one.
- `xenoteerd` is critical. Its unexpected exit stops the container so the
  runtime restart policy can create a clean state.
- x11vnc/websockify may restart with backoff without invalidating bot control.
- Crash loops use capped exponential backoff and ultimately fail, never spin.

## 8. Shutdown contract

On SIGTERM:

1. API stops accepting new connections and returns `server_draining`.
2. Controller lease stops admitting commands.
3. Current input action reaches a safe cancellation boundary within its
   configured shutdown allowance.
4. Input actor sends releases for every tracked key/button and verifies with a
   same-connection query where possible.
5. WebSocket writers send a bounded shutdown event/close frame.
6. Launched application process groups receive SIGTERM.
7. After the application grace period, surviving managed groups receive
   SIGKILL and are reaped.
8. XFCE/viewer/buses/Xvfb stop in reverse dependency order.
9. PID 1 reaps all descendants and exits with the daemon/critical-service result.

No shutdown step waits indefinitely. A dirty input reset is logged as a
structured error and exposed in final diagnostics, but cannot prevent PID 1
from honoring the container timeout.

## 9. Runtime invocation profiles

### 9.1 Development

```text
docker run --rm \
  -p 127.0.0.1:8080:8080 \
  --shm-size=2g \
  -v "$PWD/workspace:/workspace" \
  xenoteer:dev
```

The published API binds container-wide as needed for port mapping, while the
host mapping defaults to loopback. Port 5900 is never published.

### 9.2 Hardened

Expected options:

- read-only root;
- writable tmpfs for `/run`, `/tmp`, and sufficiently sized `/dev/shm`;
- persistent or ephemeral explicit home volume;
- `--cap-drop=ALL` unless a tested browser sandbox profile needs a narrowly
  justified exception;
- `--security-opt=no-new-privileges=true`, subject to Chromium sandbox testing;
- default Docker seccomp/AppArmor/SELinux confinement;
- memory, CPU, PIDs, file descriptor, and log limits;
- API TLS/auth termination at a trusted same-host or sidecar proxy.

Chromium's setuid sandbox and `no-new-privileges` can conflict. The supported
browser profile SHOULD use user-namespace plus seccomp sandboxing where the host
allows it. The test matrix must prove `chrome://sandbox` status. Do not solve a
sandbox startup failure with `--no-sandbox` in production.

## 10. Known gotchas and required tests

| Gotcha | Prevention/test |
|---|---|
| `/dev/shm` defaults to 64 MiB on many Docker setups | Startup warning below minimum; Chromium and QtWebEngine stress tests at documented size |
| Socket exists before service is usable | Protocol-level readiness notifications/probes |
| Multiple session buses | Assert one bus address in every supervised service; fail unexpected `DBUS_SESSION_BUS_ADDRESS` |
| X server accepts unauthenticated clients | Negative Xauthority test; assert `-nolisten tcp`; scan listeners |
| Saved XFCE session launches stale apps | Clear session cache and disable save in image initialization |
| Random UID cannot own `/run` | Do not claim arbitrary UID support; dedicated compatibility test when added |
| PID 1 reports success after critical child crash | Critical finish policy writes failure and halts container |
| Read-only root breaks packages writing outside declared paths | Run full end-to-end suite with read-only profile |
| Container stop kills before cleanup | Assert stop timeout exceeds service grace; forced-stop fault test |
| Host mount recursively chowned | Never boot-time chown unknown mounts; ownership error is explicit |

## 11. Deliverables

- Multi-stage Dockerfile and digest lock metadata
- s6-overlay extraction with checksum verification
- s6-rc service graph and readiness probes
- runtime directory/Xauthority/D-Bus initialization
- development and hardened Compose examples
- health endpoints and container `HEALTHCHECK`
- listener/auth/read-only-root/shutdown integration tests
- operator documentation for required `/dev/shm`, mounts, ports, and limits

## 12. Primary references

- [s6-overlay v3 documentation](https://github.com/just-containers/s6-overlay)
- [s6 readiness notifications](https://skarnet.org/software/s6/notifywhenup.html)
- [s6-rc dependency semantics](https://skarnet.org/software/s6-rc/overview.html)
- [s6-overlay arbitrary-UID `/run` caveat](https://github.com/just-containers/s6-overlay/issues/427)
- [D-Bus daemon manual](https://dbus.freedesktop.org/doc/dbus-daemon.1.html)
- [D-Bus specification](https://dbus.freedesktop.org/doc/dbus-specification.html)
- [Xvfb manual](https://www.x.org/releases/X11R7.6/doc/man/man1/Xvfb.1.xhtml)
- [Docker run security and tmpfs options](https://docs.docker.com/reference/cli/docker/container/run)
- [Docker rootless mode](https://docs.docker.com/engine/security/rootless/)
- [Docker seccomp profile](https://docs.docker.com/engine/security/seccomp/)
- [Debian snapshot service](https://snapshot.debian.org/)
- [APT snapshot sources](https://manpages.debian.org/testing/apt/sources.list.5.en.html)
- [Qt WebEngine container shared-memory warning](https://doc.qt.io/qt-6.9/qtwebengine-platform-notes.html)
- [Chromium Linux sandbox](https://chromium.googlesource.com/chromium/src/%2B/refs/tags/140.0.7339.129/sandbox/linux/README.md)
