<!-- SPDX-License-Identifier: Apache-2.0 -->

# Xenoteer Rust SDK

The Apache-2.0 Rust client for Xenoteer's frozen v1 control protocol. It uses
Rustls with platform trust roots for HTTPS/WSS, permits plaintext HTTP only for
numeric loopback origins, keeps bearer credentials out of URLs and debug
output, and never automatically replays a command mutation.

```rust,no_run
use std::time::Duration;
use xenoteer_sdk::{Command, DesktopProbeCommand, XenoteerClient};

# async fn example() -> Result<(), xenoteer_sdk::SdkError> {
let client = XenoteerClient::connect(
    "https://xenoteer.example",
    std::env::var("XENOTEER_TOKEN").map_err(|_| xenoteer_sdk::SdkError::InvalidBearerToken)?,
).await?;
let desktop = client.desktop()?;

// The caller can persist this ID before the network attempt.
let submission = desktop.submit(Command::DesktopProbe(DesktopProbeCommand {}))?;
eprintln!("command_id={}", submission.id());
let mut command = submission.send().await?;
command.wait_terminal(Duration::from_secs(30)).await?;
# Ok(())
# }
```

Important lifecycle rules:

- `CommandSubmission::id()` is available before I/O. If submission fails
  ambiguously, query that ID first; explicitly resend the same retained
  submission only when the caller's recovery policy says to do so. After
  reconnecting, call `ensure_generation()` with the refreshed desktop
  generation before lookup or explicit resend.
- Dropping a `CommandHandle` never cancels server work. A local wait timeout
  also leaves the command running; call `refresh`, wait again, or explicitly
  `cancel`.
- `Desktop`, window references, element references, leases, events, and
  artifacts are bound to one desktop generation. Treat `stale_reference` as a
  resynchronization boundary and obtain fresh references from a fresh status
  snapshot. `WindowHandle` and `ElementHandle` retain their original identity;
  `check_current()` reports reuse/restart as stale, and `relocate()` creates a
  distinct handle without mutating the old one.
- Event streams use bounded local queues, validate subscription IDs and exact
  topic filters, enforce a 1 MiB frame/message ceiling, correlate randomized
  heartbeats, and reconnect transport loss with the last processed global
  sequence. Queue overflow, permanent server errors, and peer closure are
  explicit terminal items. `ResyncRequired` is also terminal: refresh
  authoritative snapshots and explicitly create a new subscription rather
  than replaying from a guessed cursor.
- `XenoteerClient::close()` closes every derived object through shared state,
  cancels owned event supervisors, and waits for them within a bound. Calls
  made through any clone after closure fail with `ClientClosed`.
- `ControlLease::release(&mut self)` marks the local capability inactive only
  after a valid server response. A timeout or disconnect retains the exact
  capability so the caller can query or retry the ambiguous release.
- `Desktop::with_control` awaits release when its future completes normally.
  Renewal failure fences new work, allows only a bounded callback grace, and
  reports exact IDs for submissions still in flight when that callback is
  aborted. Outer cancellation, drop, or panic cannot await release; the
  server-enforced lease TTL remains the fallback. An ambiguous scoped release
  returns a redacted cleanup error whose explicit `lease_id()` accessor enables
  state inspection and exact-ID release recovery.
- Clipboard artifact upload accepts an exact-length `AsyncRead` and computes
  SHA-256 while streaming, without collecting the 16 MiB ceiling into one
  allocation.
- Artifact downloads validate scope, media type, length, and SHA-256 while
  streaming. A failed stream may have written a prefix, so atomic file users
  should write to a new temporary file and rename only after success. The v1
  Rust SDK intentionally downloads complete objects only and rejects HTTP 206;
  callers needing a range use the low-level HTTP contract directly.

The low-level `Client` remains available for applications that need exact
envelope control. Both high- and low-level clients make one transport attempt
per mutation call.

Release qualification runs the language-neutral corpus through the packaged
`xenoteer-sdk-conformance` binary. From a Xenoteer source checkout:

```sh
cargo build -p xenoteer-sdk --bin xenoteer-sdk-conformance --jobs 2
python3 scripts/conformance/run.py \
  --adapter target/debug/xenoteer-sdk-conformance
```

The adapter evaluates declarative operations through the same protocol types,
command submission, event continuity, client-close, and ambiguous-lease
lifecycle policies used by the SDK. Release runs require all 73 v1 cases to
pass with no skips.

## Installed behavior example

Every published crate includes `examples/phase6_behaviors.rs`. The release gate
copies that source from the safely extracted `.crate` into an isolated consumer
whose SDK and protocol dependencies also resolve only from extracted archives.
The example requires `XENOTEER_API_BASE`, `XENOTEER_TOKEN`,
`XENOTEER_EXPECTED_INSTALL_ROOT`, `XENOTEER_EXPECT_AUTH_FAILURE`, and
`XENOTEER_QUICKSTART_LANGUAGE`.

Against the exact derived GTK fixture image it proves the same ten behaviors as
the npm, wheel, and sdist examples: capabilities; scoped lease and registered
application launch; exact window/element resolution; semantic invoke; smooth
physical click; exact Unicode strategy evidence; screenshot after an actual
failed postcondition; reconnect by known command ID; stale reference after
restart; and exact-origin view-only browser ticket.
