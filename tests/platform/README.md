# Platform spike tests

These tests prove native backend behavior and are intentionally separate from
portable unit tests. They never accept a mocked backend as proof.

- `run-x11-spikes.sh` creates an authenticated, TCP-disabled Xvfb, keeps that
  isolated server from resetting between test binaries, runs the ignored live
  X11/XTEST/poll-loop tests, and enables the native xkbcommon model.
- `run-atspi-spike.sh` creates a session bus, AT-SPI registry, authenticated
  Xvfb, and minimal GTK fixture, then runs the ignored live AT-SPI test.
- `run-concurrent-spikes.sh` launches two X11 harnesses concurrently and then
  two AT-SPI harnesses concurrently. Per-display advisory locks are held from
  allocation through cleanup, proving the harnesses do not race on display
  selection.

Each script fails if a required executable, extension, event, mapping, fixture,
or protocol round trip is absent. The live scripts require `flock` for atomic
display allocation and release their lock descriptors only after Xvfb cleanup.
Default `cargo test` stays usable on hosts
without X11/AT-SPI development packages because native bindings are optional
features.
