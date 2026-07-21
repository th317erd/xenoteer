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

Run the Phase-0 daemon with a configuration file:

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

`/livez` proves that the Phase-0 HTTP process is alive and is the current
container/s6 health signal. `/readyz` truthfully remains `503` with internal state
`phase0_backend_probes_not_wired`; Phase 2 must wire and pass the required X11,
desktop-session, accessibility, capture, and viewer probes before it may become
`200`.

## License boundaries

The server and runtime are licensed under the repository Business Source License.
The public protocol crate, checked-in schemas, and Rust SDK are separately
licensed under Apache-2.0; see
[`crates/xenoteer-protocol/NOTICE`](crates/xenoteer-protocol/NOTICE) and
[`crates/xenoteer-sdk/NOTICE`](crates/xenoteer-sdk/NOTICE).
