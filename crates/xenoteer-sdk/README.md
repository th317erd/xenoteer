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

## Connection options and TLS

`Client::new(origin, token)` remains the static-token, native-root convenience
constructor. Advanced callers build immutable validated options and then call
`Client::from_options`:

```rust,no_run
use std::time::Duration;
use xenoteer_sdk::{Client, ClientOptions, ReconnectPolicy, TlsPolicy};

# fn configured(ca_der: Vec<u8>, client_cert_der: Vec<u8>, client_key_der: Vec<u8>)
#     -> Result<Client, xenoteer_sdk::SdkError> {
let tls = TlsPolicy::custom_roots()
    .with_root_certificate_der(ca_der)
    .with_client_identity_der(vec![client_cert_der], client_key_der);
let options = ClientOptions::builder_with_token_provider(
    "https://xenoteer.example",
    || async { obtain_rotating_token().await },
)
    .tls_policy(tls)
    .connect_timeout(Duration::from_secs(10))
    .request_timeout(Duration::from_secs(35))
    .reconnect_policy(ReconnectPolicy::new(
        5,
        Duration::from_millis(200),
        Duration::from_secs(10),
    )?)
    .client_metadata("my-controller", "1.0.0")
    .build()?;
let client = Client::from_options(options)?;
# Ok(client)
# }
# async fn obtain_rotating_token() -> Result<String, ()> { Err(()) }
```

The provider is called immediately before every HTTP attempt and every initial
or reconnecting WSS attempt. Provider errors and panic payloads are erased from SDK errors and safe logs;
resolution is bounded by the matching request or connect deadline. Provider
panics are caught so they cannot escape into or change the transport outcome.
Rust's installed panic hook runs before an unwind can be caught, however, so
panic-hook output remains the caller/runtime's responsibility; callbacks must never place secrets in panic payloads.
One Rustls
configuration supplies native and/or caller DER roots plus an optional DER mTLS
identity to both transports. Options construction validates that exact shared
configuration before it can reach a client. `connect_timeout` bounds TCP and
TLS establishment. `request_timeout` is one absolute HTTP budget shared by
credential resolution, connection/request I/O, response-body collection, and
decode; no later phase receives a fresh timeout. `Client::close()` interrupts
either credential resolution or the exchange promptly. Safe-log HTTP success
is emitted only after the complete bounded response has validated.
Reconnect uses bounded capped exponential backoff with jitter and ends with
`ReconnectExhausted` when its configured attempt budget is consumed.
Pre-welcome close codes 4401, 4403, and 1008 are permanent authentication or
permission failures. Established heartbeat and control-pong writes are bounded
by the smaller of the negotiated heartbeat and configured connection timeout,
and remain client-cancellable.

The `rustls-tls` feature is required and enabled by default. `native-roots` is
also in the default feature set but is optional: custom-root-only consumers can
build with `--no-default-features --features rustls-tls`. No OpenSSL backend is
enabled. A safe-log hook receives only closed transport/operation/outcome enums;
there is no field capable of carrying paths, headers, bodies, tokens, provider
errors, or backend prose, and hook failure or panic cannot affect transport.
As with provider callbacks, safe-log hook panics are isolated from transport,
but the installed panic hook may still render their payload.

Release-one Rust clients are deliberately direct-origin-only. They ignore
`HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY`; there is no proxy option. The
pinned Hyper and Tokio-Tungstenite clients use different connector interfaces,
and no vetted common proxy connector can currently preserve one proven Rustls
trust/client-identity policy across both HTTPS and WSS. Adding only an HTTP
proxy would create a split security policy, so proxy support requires a future
common bounded connector and parity tests rather than environment magic.

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
