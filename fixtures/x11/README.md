# X11 fixtures

This standalone crate is intentionally outside the production workspace.

- `x11-event-recorder` maps a grid window with stable `WM_NAME`/`WM_CLASS` and
  writes bounded JSONL records for motion, buttons, keys, focus, crossing, and
  lifecycle events. Key records include the core mapping's first keysym.
- `x11-color-bars` paints exact red, green, blue, white, and black 100-pixel
  bands. Together with the small raw JSON fixtures, it proves actual GetImage
  storage separately from pure decoder edge cases.
- `x11-input-driver` sends XTEST motion and follows it with `QueryPointer` on
  the same connection. The platform harness checks its endpoint and target
  window against the recorder's independently observed `MotionNotify`.

`Ready` is emitted only after the first paint and a reply-producing round trip,
so it is a synchronization boundary rather than merely a MapWindow request.

Both use the display and authentication already present in `DISPLAY` and
`XAUTHORITY`; neither disables X authentication or opens TCP.

The fixture crate has its own lockfile because it is intentionally outside the
production workspace. Run its canonical local quality gate from the repository
root:

```sh
cargo fmt --manifest-path fixtures/x11/Cargo.toml --all --check
cargo clippy --manifest-path fixtures/x11/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path fixtures/x11/Cargo.toml --all-targets --locked
cargo deny --manifest-path fixtures/x11/Cargo.toml --all-features --locked check
```

The last command deliberately reuses the repository-root `deny.toml`. CI runs
these commands independently of the production workspace gates so fixture-only
dependency or lockfile changes cannot bypass review.
