# Minimal GTK AT-SPI fixture

`minimal_gtk.py` exposes a stable application root, button, and entry through
GTK3's accessibility bridge. It has no network or test-control backdoor. The
platform harness uses the optional ready file only to distinguish process
startup from AT-SPI discovery; the Rust probe must independently find the
application through the registry.

The harness prepends `share/dbus-1/services/org.a11y.Bus.service` to its private
session's activation data and assigns that D-Bus daemon a private
`XDG_RUNTIME_DIR`. This forces direct AT-SPI bus activation inside the test
session. Without it, concurrent sessions can be redirected through the user's
systemd activation environment to the same `/run/user/.../at-spi/bus` socket,
causing cross-session GUID races.
