# Deterministic X11 desktop

## 1. Scope

This plan defines the Xvfb/XFCE desktop profile, environment, package baseline,
readiness, accessibility activation, and application compatibility settings.
Container process supervision is in [01](01-container-runtime.md); protocol-level
input and observation are in later plans.

The target is not a pixel-identical renderer across all CPU architectures and
package updates. The target is a declared display profile whose behavioral and
visual changes are detectable in CI.

## 2. Display contract

Initial fixed profile:

```text
DISPLAY=:99
screen=0
geometry=1920x1080
depth=24
dpi=96
coordinate origin=(0,0), top-left
coordinate extent=x:[0,1919], y:[0,1079]
```

Xvfb launch shape:

```text
Xvfb :99 -screen 0 1920x1080x24 -dpi 96 -nolisten tcp -auth <path>
```

The implementation may add tested server flags but must not use `-ac`. At
startup it queries root geometry, depth, visual masks, XTEST, XKB, XFIXES,
DAMAGE, SHM, Composite, and RandR availability. Capabilities are published;
extensions needed for a stable feature become readiness requirements.

### Coordinate rules

- Public root coordinates are signed 32-bit integers for API consistency.
- Validation rejects values outside the configured screen before converting to
  XTEST's signed 16-bit motion fields.
- Release one also rejects any display dimension above 32,767 because XTEST
  absolute coordinates cannot represent it.
- Rectangles are `(x, y, width, height)` with nonnegative size and half-open
  right/bottom edges.
- Screenshot and pointer coordinates are physical X root pixels; no CSS/device
  scaling abstraction is applied.
- AT-SPI coordinates are normalized from its declared coordinate system and
  returned with an explicit `coordinate_space` field.

## 3. Desktop package baseline

Install only the XFCE pieces needed for a coherent session, not a general
consumer distribution metapackage. Expected components:

- `xfce4-session`, `xfwm4`, `xfsettingsd`, `xfdesktop4`, and a minimal panel;
- `dbus`, `dbus-x11`, `at-spi2-core`, accessibility bridges pulled by GTK/Qt;
- Xvfb, X11 utilities used by probes, xauth, XKB data, fonts;
- x11vnc and viewer dependencies defined elsewhere;
- test fixture dependencies for GTK and Qt in test/development image layers.

Avoid installing display managers, NetworkManager, CUPS, Bluetooth services,
modem services, UPower-dependent UI extras, package update notifiers, keyrings,
and automounters unless a supported workflow requires them. Every daemon is a
boot-time nondeterminism and attack-surface choice.

Maintain package groups in build metadata:

- `runtime-required`
- `viewer-required`
- `browser-profile`
- `test-fixtures`
- `diagnostic-tools`

The final release image may contain diagnostics such as `xdotool`, `xprop`,
`xwininfo`, `xdpyinfo`, `xev`, and `xclip`, but Xenoteer does not shell out to
them on its normal control path.

## 4. Session environment

Every desktop process inherits a generated environment, including:

```text
DISPLAY=:99
XAUTHORITY=/run/user/1000/Xauthority
XDG_RUNTIME_DIR=/run/user/1000
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
XDG_CURRENT_DESKTOP=XFCE
XDG_SESSION_DESKTOP=xfce
XDG_SESSION_TYPE=x11
DESKTOP_SESSION=xfce
LANG=en_US.UTF-8
LC_ALL=en_US.UTF-8
TZ=UTC
GTK_MODULES=gail:atk-bridge (only if required by packaged GTK generation)
QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
NO_AT_BRIDGE=0
```

Do not blindly preserve the container environment. Generate an allowlisted
service environment. Applications launched by the daemon receive the desktop
environment plus explicitly permitted request overrides.

Use UTC and `en_US.UTF-8` by default for repeatability. Locale and timezone are
configurable only at desktop creation, not during active automation, because
changing them can alter toolkit text, layouts, and timestamps. A different
locale is a distinct test profile.

## 5. Accessibility activation

AT-SPI infrastructure must exist before applications launch.

- Start/discover the accessibility bus and registry.
- Set `org.a11y.Status.IsEnabled=true` and
  `org.a11y.Status.ScreenReaderEnabled=true` when the session implementation
  supports those status properties.
- Set `QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1` because Qt otherwise requires both
  status properties and deployments vary in how they are surfaced.
- Launch Chromium fixtures with
  `--force-renderer-accessibility=complete`; Chromium normally enables its full
  accessibility tree on demand.
- For Electron applications controlled by a cooperating launcher, enable
  accessibility before ready or supply the Chromium flag. Third-party Electron
  coverage is best effort.
- Maintain a Firefox profile pref that forces accessibility on when the packaged
  Firefox version supports it; verify by observing its AT-SPI application root,
  not by trusting the pref alone.

Accessibility increases browser CPU/memory and can produce very large trees.
This is an intentional release-one tradeoff, bounded by the cache/query limits
in [07](07-atspi-semantics.md).

## 6. XFCE determinism profile

### 6.1 Session state

- Disable "Automatically save session on logout."
- Hide/disable save-session UI where possible.
- Delete `${XDG_CACHE_HOME}/sessions/*` during image preparation and before an
  explicitly clean ephemeral boot.
- Never delete a user-provided persistent profile without an explicit reset
  operation.
- Disable XFCE's SSH/GPG agent startup; Xenoteer does not create background
  credential agents.
- Disable automatic GNOME/KDE service launch.

Xfce documents that saved-session services start in addition to configured
autostart entries. Both sources must be controlled.

### 6.2 Autostart

Create an image-owned `${XDG_CONFIG_HOME}/autostart` policy. For unwanted
system-wide entries, add same-named user override `.desktop` files with
`Hidden=true` as specified by XDG Autostart. Keep an allowlist for:

- required XFCE session components;
- accessibility infrastructure not supervised separately;
- Xenoteer-specific desktop integration, if any.

Do not rely on `OnlyShowIn` alone; package changes can add XFCE-matching entries.
An integration test inventories launched processes 10 seconds after readiness
and fails unexpected long-lived processes.

### 6.3 Window manager and compositor

Start `xfwm4 --compositor=off`. Reasons:

- no transition/opacity frames in screenshot assertions;
- no compositor shadows expanding visual bounds;
- lower CPU and less DAMAGE noise;
- fewer ARGB/redirect interactions with capture/VNC.

Disable focus-stealing prevention only to the extent required for deterministic
automation, while using EWMH activation correctly. Configure:

- click-to-focus or a tested explicit focus policy;
- no focus-follow-mouse;
- no workspace wrap;
- one workspace in release one;
- no window placement animation;
- stable titlebar theme and button layout;
- no edge tiling unless explicitly tested;
- no keyboard shortcuts that intercept common automation chords unexpectedly.

The exact xfconf property file is committed as data and snapshot-tested. Avoid
procedurally mutating many xfconf properties on every boot.

### 6.4 Blankers, lock screens, and power

Do not run `xfce4-power-manager` or a screensaver/locker unless a dependency
forces it. If installed, autostart-disable it and set all display blank/sleep/off
actions disabled. Also run `xset s off -dpms` as an idempotent defense after the
X server is ready.

No session action may suspend, hibernate, power off, or lock the container.
Power/logout UI is removed from the panel profile to prevent accidental clicks.

### 6.5 Notifications and desktop chrome

- Use a minimal, fixed panel layout or omit the panel for a `bare` profile.
- Disable update/network/power notification plugins.
- Set a static solid-color wallpaper generated as a repository asset.
- Disable desktop icon auto-arrangement changes and mounted-device icons.
- Configure a predictable notification placement and short timeout, or disable
  the notification daemon in the bare profile.
- Disable GTK overlay scrollbars and animations where supported to reduce
  transient screenshot differences.

Provide two profiles without runtime mutation:

- `standard`: XFCE desktop/panel suitable for visible demonstrations.
- `bare`: window manager, settings daemon, wallpaper, no panel/notifications;
  preferred for test automation.

## 7. Fonts, themes, and rendering

Install a deliberate, license-reviewed font set:

- DejaVu or Liberation for stable Latin metrics;
- Noto core, CJK, and emoji coverage as separately enumerated packages;
- a monospace family used by terminal fixtures.

Set default GTK and XFCE fonts, size, hinting, antialiasing, and RGBA order in
the profile. Install a fixed icon/theme set. Theme packages are dependencies
whose version changes can alter pixels; visual baselines key on the image digest.

Run `fc-cache` at build time and assert expected families through `fc-match`.
Do not download fonts on first boot. Missing glyphs are surfaced by a fixture
containing Latin, Arabic/RTL, CJK, combining marks, and emoji.

Visual assertions should prefer region/structural tolerances over whole-screen
exact hashes. Font rasterization and browser versions can legitimately shift a
small number of pixels even within the declared profile.

## 8. Browser application profiles

### 8.1 Chromium

Use a dedicated, writable profile directory under the desktop home. Required
policy:

- accessibility forced complete;
- first-run/default-browser prompts disabled;
- crash-recovery bubbles disabled;
- extensions disabled unless explicitly part of a test;
- downloads routed to a configured workspace path;
- notifications, password storage, translation, metrics, and background mode
  disabled;
- stable window size supplied or controlled after launch;
- sandbox remains enabled; `--no-sandbox` is forbidden outside a test named for
  that unsafe mode;
- GPU usage disabled initially if it makes Xvfb behavior unstable; do not claim
  WebGL/GPU compatibility in release one.

Chromium uses `/dev/shm`; the container runtime must satisfy the size contract.
Avoid papering over an undersized mount with `--disable-dev-shm-usage` as the
standard profile, because that shifts pressure and changes performance.

### 8.2 Firefox

Use a dedicated policy/profile with first-run, default-browser, telemetry,
updates, session restore, and crash UI disabled. Configure downloads and
accessibility. Firefox and Chromium profiles are ephemeral by default; persistent
profiles are explicit mounts and must carry a compatible image/profile version.

### 8.3 QtWebEngine/Electron

QtWebEngine has Chromium-like shared memory and sandbox constraints. The fixture
must prove it starts under the hardened runtime profile. Electron apps may have
custom accessibility activation and browser flags; record the actual semantic
capabilities in the launch result.

## 9. Application launch contract

Applications launch only after desktop readiness. The daemon:

1. resolves a registered application ID or an explicitly authorized executable;
2. constructs argv as an array—never a shell string;
3. clears the environment and applies desktop defaults plus allowlisted
   overrides;
4. validates working directory beneath configured roots;
5. creates a new process group;
6. captures bounded stdout/stderr or inherits according to launch policy;
7. observes PID and `/proc` start time as a process identity;
8. waits optionally for an EWMH window or AT-SPI application matching the
   launch correlation rules;
9. returns process/window/application references and partial diagnostic state.

Freedesktop `.desktop` launch support is a separate parser using the Desktop
Entry Specification's field-code/quoting rules. Do not pass an `Exec=` value to
`sh -c`. D-Bus-activatable entries may not create a child whose PID is directly
owned; correlate by bus name and window/application evidence.

## 10. Readiness and capability probe

Desktop-ready requires:

- Xvfb authenticated and correct geometry/depth;
- `xfwm4` active and `_NET_SUPPORTED` present;
- the root has the expected single-workspace configuration;
- session D-Bus and accessibility registry respond;
- test `xenoteer-desktop-probe` can create/map/name/unmap a tiny window and
  observe its EWMH lifecycle;
- pointer query and a one-pixel capture succeed;
- no locker/blanker is mapped;
- unexpected autostart process inventory is empty.

The probe window uses a reserved class/name and never steals lasting focus.

## 11. Known gotchas and acceptance tests

| Risk | Required behavior/test |
|---|---|
| Saved session resurrects apps | Reboot same home twice and assert process/window inventory |
| Focus policy changes physical target | Activate then verify `_NET_ACTIVE_WINDOW` and input recorder target |
| Compositor changes capture/shadows | Assert compositor disabled and root screenshot fixture |
| Blanker maps over desktop after idle | 30-minute idle soak with periodic capture, no input |
| Qt has no AT-SPI tree | Assert status properties/env before launch and fixture tree after launch |
| Chromium tree only appears on demand | Force complete accessibility and assert tab/document nodes |
| Browser SIGBUS | Shared-memory stress fixture at supported mount size; startup warning below it |
| Package adds autostart daemon | Boot process inventory is allowlist-tested |
| Root geometry differs from config | Readiness fails with expected/observed values |
| Locale/theme alters selectors and screenshots | Status reports profile identity; per-image visual baselines |
| X11 auth accidentally disabled | Anonymous connection negative test and listener scan |
| Window screenshots assume unobscured pixels | Document/correct behavior in observation plan; use root capture or Composite path |

## 12. Deliverables

- Fixed `standard` and `bare` XFCE profile trees
- Xvfb/Xauthority bootstrap and extension inventory
- Desktop environment manifest
- GTK, Qt, Chromium, Firefox, and QtWebEngine/Electron fixtures
- Autostart and post-readiness process inventory tests
- Font/locale/RTL/emoji fixture
- Long-idle, persistent-home, and clean-home boot tests
- Operator-selectable profile setting that is immutable after boot

## 13. Primary references

- [Xvfb manual](https://www.x.org/releases/X11R7.6/doc/man/man1/Xvfb.1.xhtml)
- [Xfce session preferences](https://docs.xfce.org/xfce/xfce4-session/preferences)
- [Xfce saved-session troubleshooting](https://docs.xfce.org/xfce/xfce4-session/faq)
- [Xfce session paths and agent settings](https://docs.xfce.org/xfce/xfce4-session/4.16/advanced)
- [xfwm4 compositor behavior](https://docs.xfce.org/xfce/xfwm4/introduction)
- [Xfce power/display settings](https://docs.xfce.org/xfce/xfce4-power-manager/preferences)
- [XDG Autostart specification](https://specifications.freedesktop.org/autostart/0.5/)
- [Desktop Entry Specification](https://specifications.freedesktop.org/desktop-entry/latest-single/)
- [Qt accessibility activation](https://doc.qt.io/qt-6/qaccessible.html)
- [Chromium accessibility flags](https://chromium.googlesource.com/chromium/src/%2B/HEAD/docs/accessibility/overview.md)
- [Electron accessibility](https://www.electronjs.org/docs/latest/tutorial/accessibility/)
- [Firefox accessibility architecture](https://firefox-source-docs.mozilla.org/accessible/Architecture.html)
- [Qt WebEngine platform/container notes](https://doc.qt.io/qt-6.9/qtwebengine-platform-notes.html)
