# Security, isolation, and abuse resistance

## 1. Security posture

Xenoteer intentionally provides remote control over a desktop. Anyone with
`input:control`, application launch, clipboard, screenshot, or artifact access
can affect or extract data from that desktop. Security therefore means:

- authenticate and authorize the controller;
- confine the desktop/container from the host and other sessions;
- minimize and bound exposed functionality;
- keep untrusted GUI content away from root/host authority;
- make viewer and automation paths share an explicit control policy;
- protect secrets/content in transport, logs, artifacts, and supply chain;
- fail closed on uncertain identity/generation.

The container is not a virtual machine. Untrusted websites/documents exercise
browser/toolkit/kernel attack surface. High-risk multi-tenant deployments should
add a VM/microVM boundary outside Xenoteer.

## 2. Threat model

### Assets

- host kernel/files/network/credentials;
- API tokens and viewer tickets;
- desktop files, browser profiles, cookies, clipboard, screenshots;
- bot actions and command results;
- proprietary Xenoteer binaries/source;
- build/signing credentials and published image integrity.

### Threat actors

- unauthenticated network client;
- authenticated principal exceeding its scope;
- compromised/buggy SDK in a loop;
- malicious web page/document/application inside desktop;
- compromised viewer browser/session URL;
- malicious package/dependency/update;
- another host/container user where runtime isolation is weak;
- accidental operator misconfiguration.

### Trust boundaries

```text
Internet -> TLS/auth gateway -> xenoteerd API
                                  |
                      desktop user process boundary
                                  |
                  X11/D-Bus apps share one trust domain
                                  |
                      container -> host kernel
```

All applications connected to one X server can potentially observe/control
other X clients under traditional X11. Xauthority prevents outsiders from
connecting; it does not isolate mutually hostile in-desktop apps. Release one
supports one trust domain per container.

## 3. Default network exposure

- Xvfb uses Unix socket and `-nolisten tcp` with Xauthority.
- D-Bus uses Unix sockets only.
- x11vnc and websockify bind 127.0.0.1 inside container.
- Only the authenticated HTTP/WS API listener is intended for publication.
- API auth is mandatory by default, even if a host maps the port to loopback.
- TLS/WSS is mandatory over untrusted networks; plaintext is acceptable only on
  a protected loopback/Unix/sidecar network with documented termination.
- `/livez` and `/readyz` expose booleans only; no version/config internals.
- No UDP listener, discovery broadcast, mDNS, or remote D-Bus/X11.

CI starts the image and scans `/proc/net`/`ss` plus an external network namespace
to assert listeners/reachability. A package update that launches a daemon fails
the test.

## 4. Authentication

Release-one built-in provider: high-entropy bearer tokens read from 0600 files
or generated into a runtime 0600 file. Requirements:

- at least 256 random bits;
- token value never logged, put in URL, CLI argv, status, crash report, or image;
- compare a strong keyed hash/constant-time representation, not plaintext string
  with timing-variable early exit;
- multiple token records permit rotation and distinct principals;
- records define token ID, principal ID, capabilities, desktop scope, created/
  expires/revoked metadata;
- file reload is atomic: parse/validate complete replacement then swap;
- invalid reload retains old valid config and raises operator alert; expired
  token fails closed.

Do not invent password login, OAuth, or JWT issuance. Deployments needing SSO use
an external identity-aware proxy and pass a signed/authenticated identity only
over a mutually trusted channel with exact proxy allowlist. Never trust arbitrary
`X-Forwarded-*`/identity headers from clients.

## 5. Authorization

Capability checks occur before resource lookup and again near queued effects:

| Capability | Allows |
|---|---|
| `desktop:status` | detailed health/capabilities |
| `desktop:observe` | window/process metadata and events |
| `input:control` | lease and physical mouse/keyboard |
| `clipboard:read` / `clipboard:write` | selection content operations |
| `accessibility:read` | semantic tree/query/events |
| `accessibility:write` | invoke/focus/value/text/selection |
| `application:launch` | registered/allowed executable launch |
| `application:terminate` | managed process group termination |
| `capture:read` | screenshots |
| `artifact:read` / `artifact:delete` | authorized generic trace/support artifacts; purpose-specific artifacts also require their originating capability |
| `viewer:read` | view-only ticket |
| `advanced:raw_input` | raw keycodes/down/up/mapping strategy |

Scope includes desktop ID/generation and allowed application/workspace roots.
References are not capabilities; possessing a `WindowRef` does not authorize it.

Protected-field text mutation, temporary keymap, arbitrary binary clipboard
targets, custom executable paths, and force kill may have sub-capabilities/policy
flags because their risks exceed ordinary actions.

There is no broad artifact-write capability in release one. The artifact upload
route accepts only the `clipboard_input` purpose under `clipboard:write`, keeps
the object outside `/workspace`, and never turns its name or contents into an
application-visible filesystem path.

## 6. API abuse resistance

Apply layered limits from [10](10-api-and-protocol.md): request bytes, message
rate, concurrent commands, queue capacity, query nodes, screenshot pixels,
clipboard bytes, processes, stdout, artifacts, subscriptions, and durations.

Additional rules:

- Parse under body limit; never buffer unlimited request before checking.
- Unknown fields rejected; regex uses bounded linear-time engine.
- UUIDs and opaque cursors have fixed maximum representation.
- Result/event queues cannot be driven unbounded by a disconnected client.
- Authentication failures have strict rate limit and minimal detail.
- `Retry-After` prevents SDK hot loops; SDK also caps retry.
- Command dedupe prevents a retry storm from multiplying effects but ledger
  itself is bounded per principal/global.
- Health routes have their own cheap rate limit and never run deep probes per
  request; they read supervisor snapshot.

OWASP API risks most relevant here: broken object/function authorization,
authentication, unrestricted resource consumption/sensitive flows, security
misconfiguration, and inventory drift. Test each explicitly.

## 7. Container runtime hardening

### 7.1 Required baseline

- No `--privileged`.
- Desktop/daemon/application processes run non-root.
- No host Docker/container runtime socket.
- No host PID/network/IPC namespace by default.
- No host `/`, home, SSH agent, cloud credential, `/dev/input`, GPU, or X socket
  mounts.
- Default Docker seccomp and LSM profile retained; custom changes are additive
  restrictions or narrowly justified exceptions.
- Drop Linux capabilities; target `cap_drop: ALL` after browser/s6 compatibility
  testing.
- Resource limits: memory, CPU, PIDs, files, file descriptors, tmpfs, logs.
- Read-only root profile with explicit writable tmpfs/volumes.
- `no-new-privileges` where compatible with selected Chromium sandbox mode.

Docker documents that `--privileged` grants all capabilities, host devices, and
disables seccomp/LSM confinement; Xenoteer never recommends it as troubleshooting.

### 7.2 s6/root transition

s6 starts as root to prepare runtime paths and drops to `xenoteer` for services.
Initialization scripts are image-owned, non-writable by desktop user, simple,
idempotent, and never evaluate untrusted environment as shell code. Avoid
dynamic recursive chown on mounted paths.

If the final hardened profile can run PID 1 without root while preserving `/run`,
browser sandbox, ownership, and service behavior, add it as a separately tested
profile. Do not claim it before arbitrary-UID caveats are solved.

## 8. Browser/toolkit sandboxing

Chromium uses multiple Linux sandbox layers including namespaces and seccomp.
`--no-sandbox` disables critical protection and is forbidden for production/open
web browsing. Supported profile must prove active sandbox status.

Container conflicts to test:

- Docker seccomp may deny namespace creation needed by Chromium user-namespace
  sandbox;
- `no-new-privileges` prevents setuid elevation used by the legacy setuid helper;
- host sysctl/LSM may restrict unprivileged user namespaces;
- packaged Chromium may expect a correctly owned mode-4755 sandbox helper;
- root-launched browser often demands `--no-sandbox`, which is why apps run as
  desktop user;
- small `/dev/shm` causes crashes unrelated to sandbox.

Preferred order:

1. non-root browser with user-namespace + seccomp sandbox under default container
   confinement;
2. packaged setuid helper only if image ownership/mode and `no-new-privileges`
   profile are deliberately managed;
3. fail browser capability with diagnostic if host prevents both;
4. never silently add `--no-sandbox`.

The operator guide lists host requirements and a `doctor browser-sandbox` probe.

## 9. Application launch and process control

Default launch is registry/allowlist based:

```text
application id -> fixed executable path -> allowed argument schema
               -> allowed env keys -> allowed cwd roots -> resource policy
```

Advanced arbitrary-executable launch is disabled unless capability/config both
allow it. Even then:

- argv array only; no shell interpolation;
- executable must be regular file under allowlisted roots, with symlink/canonical
  path policy and ownership checks;
- cwd/file paths remain under allowed roots using safe directory-relative open
  where possible;
- environment cleared and rebuilt; deny `LD_PRELOAD`, `LD_LIBRARY_PATH`,
  `PYTHONPATH`, loader/debug/injection variables unless application profile owns
  them;
- no setuid/setcap executables under arbitrary path policy;
- process group, count/PID limits, output/timeout, and reaping enforced;
- terminate only a managed, revalidated process group; no arbitrary PID kill.

Generic shell/terminal APIs are not exposed by default. An automation user can
interact with a visible terminal application physically, but that is distinct
from remote command execution in the daemon.

## 10. Filesystem and workspace

- Image root and configuration are read-only to desktop user.
- `/workspace` is the only default bot/application exchange root.
- Browser downloads/uploads and file chooser helpers point there.
- Persistent browser/home profile is opt-in; ephemeral home is safer/default.
- Artifact store has separate root not traversable through application file APIs.
- Paths in API are relative logical paths; server joins beneath allowed root and
  prevents `..`, absolute, NUL, symlink, hardlink, and mount escape according to
  operation.
- File overwrite/delete APIs are deferred; launch can consume paths and downloads
  can create files under application policy.

Mounting secrets into the desktop user's readable home makes them available to
malicious GUI content. API auth/build secrets should use daemon-only ownership or
external gateway and never be exposed to launched applications.

## 11. X11/D-Bus isolation

- MIT-MAGIC-COOKIE Xauthority 0600 and `-nolisten tcp`; never `xhost +`/`-ac`.
- D-Bus session/runtime directories 0700 and Unix socket.
- Do not expose system D-Bus host socket.
- Applications share the desktop cookie/bus by design; treat them as same tenant.
- X11 SECURITY extension is not relied upon as the primary isolation mechanism;
  many required extensions/desktop interactions assume trusted clients.
- A malicious app can spoof titles/PIDs/properties/AT-SPI data. Those are target
  selectors/evidence, not authorization.
- Event/property lengths and D-Bus payloads are untrusted and bounded.

For mutually untrusted applications, use separate Xenoteer containers/X servers.

## 12. Viewer security

- x11vnc is server-side view-only.
- Raw RFB/websockify listener loopback only.
- Authenticated viewer gateway uses WSS and exact origin.
- Viewer ticket: random, short-lived, single-use, audience/origin/principal/
  desktop-generation bound; stored hashed if server-side lookup.
- noVNC clipboard/file transfer/input disabled.
- Viewer page has restrictive CSP, no third-party scripts/fonts/analytics, hashed
  static assets, `Referrer-Policy: no-referrer`, `Cache-Control: no-store` for
  ticket-bearing bootstrap, clickjacking protection (`frame-ancestors`).
- Do not place long-lived ticket in query if fragment/postMessage/bootstrap can
  avoid it; if WebSocket needs query, consume immediately and redact access logs.

x11vnc is unmaintained and processes attacker-influenced RFB messages from the
local gateway. Pin it, scan LibVNC advisories, minimize flags, and plan replacement.

## 13. Clipboard, screenshots, accessibility, logs

These are data-exfiltration capabilities, separately authorized. Defaults:

- no clipboard/text/screenshot contents in logs;
- no automatic screenshots except configured failure artifacts;
- protected accessibility text redacted and read denied;
- window titles/application args can contain secrets, so logs truncate/hash or
  omit by default;
- artifacts private, authenticated, expiring, size bounded;
- support bundles opt-in and list included fields before creation;
- metrics never use content, command IDs, titles, paths, or selectors as labels.

Structured tracing has a redaction layer and review test that plants canary
tokens in text/clipboard/title/path/auth and scans logs/errors/artifacts metadata.

## 14. Secrets

- Runtime token/TLS/proxy secrets from files with ownership/mode checks.
- Build secrets through BuildKit secret mounts; never ARG/ENV/layer/provenance.
- Secret types redact Debug/Display/Serialize.
- Environment inventory/status allowlist omits secret values.
- Core dumps disabled for daemon/browser profiles handling secrets unless an
  explicitly protected debug environment enables them.
- Panic hook records opaque IDs/backtrace policy, not request bodies/environment.
- Token rotation and revocation documented; viewer tickets naturally expire.

Memory zeroization is best-effort for narrow secret buffers; do not claim whole
process/clipboard strings are provably erased under Rust allocator/copies.

## 15. Supply-chain security

- Pin base by digest; pin downloads by version+SHA-256/signature.
- Cargo.lock committed; no unpinned git dependencies.
- GitHub Actions third-party actions pinned to immutable commit SHA.
- Minimal workflow permissions; release uses OIDC/keyless signing, no long-lived
  registry/signing key when feasible.
- Build produces SBOM and provenance; image signed/attested by digest.
- `cargo-deny`/advisory scan, container OS scan, secret scan, license scan.
- Rebuild regularly for security updates; snapshot date visible.
- Publish checksums/signatures and verification instructions.
- Release artifacts originate from protected tag/workflow, not developer laptop.

Do not make vulnerability count alone a gate: define severity, reachability,
fix availability, exception owner, expiry, and compensating control.

## 16. Logging/audit

Audit security-sensitive state changes:

- auth success/failure (no token);
- lease acquire/revoke/expiry;
- application launch/terminate by registered ID and safe args metadata;
- clipboard read/write metadata (bytes/hash/target, no content);
- screenshot/artifact/viewer ticket creation/access;
- advanced/raw input/temp keymap;
- configuration reload and capability degradation;
- authorization denial and rate limit.

Audit events include timestamp, principal ID, request/command ID, desktop
generation, action/outcome/effect stage, and source connection metadata under
privacy policy. Logs are integrity-protected/collected by deployment; Xenoteer
does not build a bespoke tamper-proof log store in release one.

## 17. Incident/failure response

- Authentication config invalid -> keep last valid or refuse startup; never
  fall back to unauthenticated.
- Input actor poisoned -> readiness false for input, revoke lease, recommend
  container restart.
- X/desktop generation changed -> invalidate all sessions/refs/tickets.
- Viewer backend vulnerability -> ability to disable viewer while API remains
  usable.
- Critical dependency advisory -> scheduled rebuild plus documented kill switch/
  image revocation.
- Suspected token leak -> revoke/rotate provider record; existing WS sessions for
  that principal close.
- Resource exhaustion -> reject/coalesce/terminate offending session before OOM;
  container limits remain last boundary.

## 18. Security verification checklist

- Threat model reviewed at every new capability.
- Auth off + non-loopback fails startup.
- Object/function authorization matrix has negative tests.
- Port/listener scan and unauthenticated X/D-Bus/RFB tests pass.
- Container works without privileged/host namespaces/devices/socket.
- Browser sandbox status proves required layers; production flags contain no
  `--no-sandbox`.
- Read-only-root/cap-drop/no-new-privileges profiles tested and documented.
- Path traversal/symlink/PID reuse/command injection tests pass.
- Fuzzers cover untrusted parsers and length fields.
- Canary secret never appears in logs/status/problem/metrics.
- SBOM/provenance/signature and source obligations published.
- x11vnc can be disabled/replaced without protocol break.

## 19. Primary references

- [Docker run security behavior](https://docs.docker.com/reference/cli/docker/container/run)
- [Docker seccomp profiles](https://docs.docker.com/engine/security/seccomp/)
- [Docker rootless mode](https://docs.docker.com/engine/security/rootless/)
- [Chromium Linux sandbox design](https://chromium.googlesource.com/chromium/src/%2B/refs/tags/140.0.7339.129/sandbox/linux/README.md)
- [Chromium user-namespace/AppArmor restrictions](https://chromium.googlesource.com/chromium/src/%2Bshow/133.0.6943.53/docs/security/apparmor-userns-restrictions.md)
- [X11 Security Extension discussion](https://www.x.org/releases/X11R7.7/doc/xextproto/security.html)
- [OWASP API Security project](https://owasp.org/www-project-api-security/)
- [Docker build attestations](https://docs.docker.com/build/metadata/attestations/)
- [Sigstore keyless signing overview](https://docs.sigstore.dev/cosign/signing/overview/)
- [Cosign verification](https://docs.sigstore.dev/cosign/verifying/verify/)
- [x11vnc upstream status](https://github.com/LibVNC/x11vnc)
- [LibVNCServer security advisories](https://github.com/LibVNC/libvncserver/security)
