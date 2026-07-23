# Xenoteer

Xenoteer is a bot-controlled Linux/X11 desktop runtime. The implementation is
being built from the design corpus in [`plans/`](plans/README.md).

## Rust development

The repository pins Rust in `rust-toolchain.toml`. From a fresh clone:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo run --locked -p xenoteer-protocol --bin generate-schemas -- --check
cargo deny check
```

Regenerate protocol schemas after an intentional protocol change:

```sh
cargo run -p xenoteer-protocol --bin generate-schemas
git diff -- schemas/
```

Run the daemon with a configuration file. Outside the supported container it
will expose liveness while desktop capability probes remain non-ready unless an
authenticated X11/XFCE/AT-SPI session is available:

```sh
cargo run -p xenoteerd -- --config ./xenoteer.toml
```

Start from [`xenoteer.example.toml`](xenoteer.example.toml). Configuration is
loaded with `compiled defaults < TOML < XENOTEER__SECTION__FIELD environment <
CLI` precedence. Unknown fields and unsafe cross-field combinations stop startup;
for example, authentication cannot be disabled on a non-loopback listener.
Unrelated environment variables are ignored, while malformed variables beginning
with `XENOTEER_` fail closed without reproducing their names or values in errors.
The documented desktop keys are intentionally fixed in release one at
1920x1080x24 and 96 DPI.

Protocol problem codes use `deadline_exceeded_before_effect` and
`deadline_exceeded_after_effect`. Command lifecycle states retain the shorter
`deadline_before_effect` and `deadline_after_effect` names because they describe
terminal state rather than the error catalog.

The implemented Phase 5 HTTP contract, WebSocket message inventory,
least-privilege grant mapping, and complete wire examples are in
[`docs/api/v1/`](docs/api/v1/README.md). Generated typed JSON Schemas remain in
[`schemas/v1/`](schemas/v1/).

The public API now includes generation-fenced window list/query/snapshot/
resolve/wait, clipboard read, screenshot capture, private artifact
upload/download/range/delete, and the origin-bound one-time-ticket viewer flow.
Phase 5 adds bounded accessibility list/query/resolve/snapshot/wait routes,
protected-field redaction, normalized accessibility events, semantic actions,
semantic text insertion, and geometry-revalidated physical element clicks with
interpolated pointer motion. Mutations are typed variants of the existing
command endpoint—not separate REST routes. The checked-in OpenAPI path inventory
is statically compared with the current server router sources by
`python3 scripts/api/validate-docs.py`.

`/livez` proves only that the HTTP process is alive. `/readyz` becomes `200`
only after the Phase 2 supervisor proves the fixed authenticated X11 display,
XFCE/EWMH lifecycle, native input actor, one-pixel capture, and AT-SPI registry.
When the viewer is configured as required, readiness also completes a bounded
WebSocket upgrade through websockify and completes RFB 3.8 negotiation through
TigerVNC's bounded `ServerInit`. Losing a required capability makes readiness
fail and asks s6 to stop the container. An unavailable enabled-but-optional
viewer reports `200 {"status":"degraded"}` without falsely taking down the
control plane, and returns to `ready` after recovery.

## Container runtime

Build and verify the deterministic Debian/XFCE image with:

```sh
sudo scripts/container/build.sh
sudo scripts/container/test-image.sh xenoteer:dev
```

The image runs Xvfb, one session D-Bus, AT-SPI, deterministic bare or standard
XFCE, the Rust daemon, and a loopback-only server-side view-only noVNC chain
under s6-overlay. Only the control-plane port is exposed. Exact package,
download, final-filesystem, and licensing evidence is embedded in the image;
the complete development, hardened, browser/toolkit, and acceptance workflow is
documented in [`container/README.md`](container/README.md).

## License boundaries

The server and runtime are licensed under the repository Business Source License.
The public protocol crate, checked-in schemas, API documentation/examples, and
Rust SDK are separately licensed under Apache-2.0; see
[`crates/xenoteer-protocol/NOTICE`](crates/xenoteer-protocol/NOTICE) and
[`crates/xenoteer-sdk/NOTICE`](crates/xenoteer-sdk/NOTICE), and
[`docs/api/NOTICE`](docs/api/NOTICE).
