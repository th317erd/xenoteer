# Phase-0 noVNC viewer spike

## Decision

The measured release-one observation chain is:

```text
Xvfb :199 (Unix socket, Xauthority)
  <- X0tigervnc 127.0.0.1:5900 (server-enforced view-only)
  <- websockify 127.0.0.1:6080 (RFC 6455 binary frames)
  <- pinned noVNC static client
```

`X0tigervnc` is supplied by Debian's TigerVNC scraping-server package. It
attaches to the existing Xvfb display; it does not create a second desktop.
The package is maintained in Debian and its explicit input/clipboard switches
make the view-only policy executable and testable.

The spike uses exact Debian packages from the same signed
`20260719T000000Z` snapshot as the Phase-0 base image:

| Component | Exact binary package | Role |
|---|---|---|
| Chromium | `150.0.7871.124-1~deb13u1` (`amd64`) | Execute the actual noVNC JavaScript/browser client |
| Chromium sandbox | `150.0.7871.124-1~deb13u1` (`amd64`) | Preserve the setuid sandbox; `--no-sandbox` is forbidden |
| TigerVNC scraping server | `1.15.0+dfsg-2.1~deb13u1` (`amd64`) | Existing-X-display to RFB adapter (`X0tigervnc`) |
| websockify | `0.12.0+dfsg1-4+b1` (`amd64`) | WebSocket-to-RFB bridge and static server |
| noVNC | `1:1.6.0-2` (`all`) | Browser client assets |
| xclip | `0.13-4` (`amd64`) | Independent clipboard effect observer |
| xdotool | `1:3.20160805.1-5.1` (`amd64`) | Focus the recorder so RFB KeyEvent has a valid target |

This avoids an unpinned Git checkout, reuses Debian's signed index and package
hash verification, and preserves Debian copyright files. `packages.lock` pins
every direct `.deb` SHA-256. `critical-assets.sha256` checks the browser entry
point and protocol modules before mandatory local configuration is installed.
The image records hashes for every installed noVNC file, a binary-to-source
`dpkg` manifest, and the exact distinct Debian copyright license stanzas for
every direct spike package under `/usr/share/doc/xenoteer-novnc-spike/`.

This is a disposable conformance image, not the release runtime stage. It stays
separate from the main Dockerfile and Compose profiles until the authenticated
viewer gateway and supervised services are implemented.

## Security properties proved

The enforcement boundary is `X0tigervnc`, not noVNC's UI. The server is started
with these mandatory semantics, and the harness verifies the real process
command line:

- `-interface 127.0.0.1`, `-localhost=1`, and fixed `-rfbport 5900`;
- `-SecurityTypes=None` only for this unreachable same-container loopback hop;
- `-AlwaysShared=1` and `-DisconnectClients=0` for observation clients;
- `-AcceptKeyEvents=0` and `-AcceptPointerEvents=0`;
- `-AcceptSetDesktopSize=0` so a viewer cannot mutate desktop geometry;
- `-AcceptCutText=0` and `-SendCutText=0` in both clipboard directions;
- `-MaxCutText=1024` as defense in depth even though cut text is disabled.

noVNC's `mandatory.json` also fixes `view_only=true`, but that is defense in
depth and is never accepted as the sole evidence. websockify binds exactly
`127.0.0.1:6080`. Neither 5900 nor 6080 is declared as an OCI exposed port, and
the executable test runs with Docker `--network none`.

RFB `SecurityTypes=None` and websockify's unauthenticated listener are safe only
while both remain on inaccessible container loopback. Before any external
viewer route exists, the same-origin Xenoteer gateway must terminate WSS,
authenticate an origin-bound short-lived viewer ticket, bind it to the current
desktop generation, and proxy to websockify without publishing either internal
port.

All X-facing processes run as fixed desktop UID 1000. Xvfb uses an explicit
private Xauthority cookie and `-nolisten tcp`; neither `-ac` nor ambient host X11
is involved.

## Executable evidence

The gate launches pinned Chromium against a small harness that imports the
actual pinned noVNC `core/rfb.js`. It polls the live browser through loopback
DevTools until noVNC reports the RFB desktop name and creates an 800x600 canvas,
then captures a bounded PNG through the browser. Chromium and every renderer
run as UID 1000 with seccomp filter mode 2. The test rejects `--no-sandbox` and
`--disable-dev-shm-usage` from the real process command lines.

The container uses a pinned Docker-default seccomp baseline extended only for
`clone`, `setns`, and `unshare`, the user-namespace operations required by the
installed Chromium sandbox. A structural gate proves that exact three-syscall
delta and rejects hidden high-risk additions. It never uses
`seccomp=unconfined`, `SYS_ADMIN`, or a browser sandbox-disable flag.

`/dev/shm` is a private 4 GiB mount. Debian Chromium 150 injects the rejected
`--disable-dev-shm-usage` flag below exactly 4,080,218,931 available bytes, so
the earlier undersized mount proposal is incompatible with the executable flag
policy. The harness verifies both the 4 GiB runtime setting and the absence of
that injected flag.

`rfb_websocket_probe.py` is a second protocol client, not a fake viewer backend.
It:

1. performs a real HTTP/1.1 WebSocket upgrade with a random key and validates
   the RFC 6455 accept value and `binary` subprotocol;
2. exchanges real masked WebSocket binary frames with websockify;
3. negotiates RFB 3.8 and checks ServerInit geometry against Xvfb;
4. requests raw encoding and proves the pixel stream contains the mapped
   recorder's black/white framebuffer content;
5. sends real RFB KeyEvent, PointerEvent, ClientCutText, and SetDesktopSize
   messages.

An independent x11rb event recorder is mapped before `X0tigervnc` starts. The
harness asserts that it sees no key, pointer, or button event after those RFB
attempts and that focus is unchanged. For resize denial, the client advertises
the RFB ExtendedDesktopSize pseudo-encoding, sends SetDesktopSize, and requires
TigerVNC's ordered `reason=client`, `result=prohibited` response to repeat the
1920x1080 ServerInit geometry. The client remains connected at that explicit
protocol barrier while the harness independently proves X11 geometry is still
1920x1080, eliminating a check-before-processing race. It
also keeps real X11 CLIPBOARD and PRIMARY ownership in a sentinel armed with a
runtime-unique secret canary. The gate identifies the requestor windows rather
than conflating TigerVNC with xfsettingsd: both request `TARGETS`, and the
sentinel answers with protocol-correct capability metadata. With
`SendCutText=0`, TigerVNC must not follow that lookup with `UTF8_STRING`, `TEXT`,
or `STRING`; therefore the sentinel must report zero canary-bearing responses.
The connected RFB client independently rejects every `ServerCutText` and proves
that the exact armed canary was not received. Serving the canary to TigerVNC
would require enabling clipboard egress or fabricating an invalid selection
response, which would weaken or alter the production semantics the gate is
meant to prove. The separate ClientCutText attempt proves the reverse direction
is denied without displacing the sentinel. Finally, direct XTEST motion and key
events are sent as positive controls and must be observed by the same recorder.
This distinguishes effective server enforcement from a recorder that could not
observe input at all.

The gate also verifies actual listener addresses through `/proc/net/tcp*`,
actual UIDs and mandatory flags through `/proc`, served noVNC files byte-for-byte
against installed pinned assets, mandatory client settings, and presence of
license/inventory artifacts.

From the repository root, after building `xenoteer:phase0`:

```sh
sudo scripts/container/test-novnc-spike.sh
```

Image overrides remain explicit:

```sh
sudo env \
  XENOTEER_NOVNC_SPIKE_BASE_IMAGE=xenoteer:phase0 \
  XENOTEER_NOVNC_SPIKE_IMAGE=xenoteer:novnc-spike \
  scripts/container/test-novnc-spike.sh
```

The gate admits the selected local base only when Docker reports both one exact
lowercase image ID and a durable pre-existing non-dangling tag or digest. It
reserves a strongly random temporary local tag for Dockerfile `FROM` and a
private mode-0700 IID directory. The IID path must be absent immediately before
the build; Docker creates it inside that anchored directory, and the gate
validates the child before reducing its permissions to mode 0600 and reading it.
The IID file binds the proof to this build invocation. A classic image store
must report the tagged output's exact image ID in that file; a containerd image
store must report the `config.digest` annotation in the tagged output's
manifest descriptor, whose digest must equal the tagged output's exact image
ID. After proving that identity is distinct and has the complete base layer
prefix, every inspect and run uses only the frozen exact output ID. The
temporary alias is
removed only after Docker again proves another durable source reference. A
validated container name is recorded before launch so HUP, INT, or TERM can
terminate and reap the Docker client, remove the runtime container, and clean
the alias. Collision, identity drift, source-reference loss, malformed IID
state, or cleanup failure makes the gate fail closed.

## Rejected measured alternative: x11vnc

The initial spike evaluated Debian x11vnc `0.9.17-1` with LibVNCServer
`0.9.15+dfsg-1+deb13u2`. A real noVNC Chromium client reached websockify, but
the raw TCP connection from websockify deadlocked with LibVNCServer's built-in
WebSocket autodetection and eventually closed with
`webSocketsHandshake: unknown connection error`; no RFB ServerInit was
delivered. The upstream x11vnc project also declares itself unmaintained.

This is an executable compatibility failure for the exact pinned stack, not a
general claim that x11vnc can never work. A timing shim was rejected because it
would create brittle, unsupported protocol behavior. Keep this result as
decision evidence; do not ship x11vnc in the selected chain.

## Risks and follow-up gates

- noVNC CSP, gateway origin/ticket behavior, TLS/WSS, local cursor, scaling UX,
  reconnect, two-viewer, slow-network, crash/restart, and eight-hour soak gates
  remain open.
- TigerVNC still processes attacker-influenced RFB after the future gateway.
  Keep it replaceable, patch promptly, and never publish its listener directly.
- Re-audit the pinned seccomp baseline whenever Docker or the browser changes;
  the only Xenoteer-specific unconditional allow remains `clone`, `setns`, and
  `unshare`.
- Re-run the real RFB input/clipboard attempts and listener/flag checks for
  every TigerVNC version change; configuration acceptance is not proof of
  effective enforcement.
- noVNC contains MPL-2.0 files plus BSD, Expat, OFL, CC-BY-SA, and Zlib works;
  websockify and TigerVNC are multi-license aggregates; xclip is
  GPL-2.0-or-later. A release must ship exact corresponding source and notices
  required by `plans/16-dependencies-and-licensing.md`.
- Debian's exact `tigervnc-scraping-server` copyright records these license
  stanzas: BSD-3-Clause, BSD-style-descipher, GPL-2+, GPL-3+, LGPL-2.1+,
  MIT/X11-style, fsfap, and public-domain. Preserve that file and do not reduce
  the aggregate to one guessed SPDX expression.

Primary upstream references:

- <https://github.com/TigerVNC/tigervnc/tree/v1.15.0>
- <https://tigervnc.org/doc/X0tigervnc.html>
- <https://github.com/novnc/websockify>
- <https://github.com/novnc/noVNC>
- <https://github.com/novnc/noVNC/blob/master/docs/API.md>
- <https://www.rfc-editor.org/rfc/rfc6455>
- <https://github.com/LibVNC/x11vnc> (rejected alternative evidence)
