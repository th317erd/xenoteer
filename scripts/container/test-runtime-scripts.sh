#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
runtime_init="$repo_root/container/rootfs/usr/local/libexec/xenoteer/init-runtime"
auth_init="$repo_root/container/rootfs/usr/local/libexec/xenoteer/init-xauthority"
probe_x11="$repo_root/container/rootfs/usr/local/libexec/xenoteer/probe-x11"
probe_session_dbus="$repo_root/container/rootfs/usr/local/libexec/xenoteer/probe-session-dbus"
probe_atspi="$repo_root/container/rootfs/usr/local/libexec/xenoteer/probe-atspi"
probe_xfce="$repo_root/container/rootfs/usr/local/libexec/xenoteer/probe-xfce"
probe_x0tigervnc="$repo_root/container/rootfs/usr/local/libexec/xenoteer/probe-x0tigervnc"
probe_websockify="$repo_root/container/rootfs/usr/local/libexec/xenoteer/probe-websockify"
test_root=$(mktemp -d)
mock_bin=$(mktemp -d)
trap 'rm -rf -- "$test_root" "$mock_bin"' EXIT
export CONTAINER_TEST_UID
export CONTAINER_TEST_GID
export CONTAINER_TEST_DAEMON_UID
export CONTAINER_TEST_DAEMON_GID
export CONTAINER_TEST_BROKER_UID
export CONTAINER_TEST_TOKEN_UID
export CONTAINER_TEST_TOKEN_GID
CONTAINER_TEST_UID=$(id -u)
CONTAINER_TEST_GID=$(id -g)
CONTAINER_TEST_DAEMON_UID=$CONTAINER_TEST_UID
CONTAINER_TEST_DAEMON_GID=$CONTAINER_TEST_GID
CONTAINER_TEST_BROKER_UID=$CONTAINER_TEST_UID
CONTAINER_TEST_TOKEN_UID=$CONTAINER_TEST_UID
CONTAINER_TEST_TOKEN_GID=$CONTAINER_TEST_GID

mkdir -p "$test_root/home/xenoteer" "$test_root/workspace" "$test_root/run/secrets"
printf '%064d' 0 >"$test_root/run/secrets/xenoteer_api_token"
chmod 0400 "$test_root/run/secrets/xenoteer_api_token"
future_value=$'first line\nsecond line\n'
CONTAINER_TEST_ROOT="$test_root" \
DISPLAY=:77 \
XENOTEER__LOGGING__FILTER='info,xenoteerd=trace' \
XENOTEER__FUTURE_SECTION__FUTURE_FIELD="$future_value" \
  "$runtime_init"

test "$(cat "$test_root/run/xenoteer/env/DISPLAY")" = :77
test "$(cat "$test_root/run/xenoteer/env/XVFB_SCREEN_GEOMETRY")" = 1920x1080x24
test "$(cat "$test_root/run/xenoteer/env/XENOTEER__SERVER__LISTEN")" = 0.0.0.0:8080
test "$(cat "$test_root/run/xenoteer/env/XENOTEER__LOGGING__FILTER")" = 'info,xenoteerd=trace'
cmp "$test_root/run/xenoteer/env/XENOTEER__FUTURE_SECTION__FUTURE_FIELD" \
  <(printf '%s\n' "$future_value")
test ! -e "$test_root/run/xenoteer/env/XENOTEER_HTTP_ADDR"
test "$(find "$test_root/run/xenoteer/env" -maxdepth 1 -type f -name 'XENOTEER_*' -printf '%f\n' | LC_ALL=C sort)" = \
  $'XENOTEER__FUTURE_SECTION__FUTURE_FIELD\nXENOTEER__LOGGING__FILTER\nXENOTEER__SERVER__LISTEN'
test ! -e "$test_root/run/xenoteer/env/XENOTEER__AUTH__TOKEN_FILE"
test "$(stat -c '%a:%u:%g' "$test_root/run/xenoteer/api-token")" = \
  "400:$CONTAINER_TEST_TOKEN_UID:$CONTAINER_TEST_TOKEN_GID"
cmp "$test_root/run/secrets/xenoteer_api_token" "$test_root/run/xenoteer/api-token"
test "$(stat -c '%a' "$test_root/run/user/1000")" = 710
test "$(stat -c '%a' "$test_root/run/user/1000/at-spi")" = 710
test "$(stat -c '%a' "$test_root/run/user/1000/xdg/config")" = 700
test "$(stat -c '%a' "$test_root/run/user/1000/xdg/cache")" = 700
test "$(stat -c '%a' "$test_root/run/user/1000/xdg/data")" = 700
test "$(stat -c '%a' "$test_root/run/user/1001")" = 700
test "$(stat -c '%a' "$test_root/run/user/1001/home")" = 700
test "$(stat -c '%a' "$test_root/run/user/1001/xdg/config")" = 700
test "$(stat -c '%a' "$test_root/run/user/1001/xdg/cache")" = 700
test "$(stat -c '%a' "$test_root/run/user/1001/xdg/data")" = 700
test "$(stat -c '%a:%u:%g' "$test_root/run/xenoteer/processd")" = \
  "750:$CONTAINER_TEST_BROKER_UID:$CONTAINER_TEST_DAEMON_GID"
test "$(stat -c '%a:%u:%g' "$test_root/run/xenoteer/bus")" = \
  "710:$CONTAINER_TEST_UID:$CONTAINER_TEST_GID"
test "$(stat -c '%a:%u:%g' "$test_root/run/xenoteer/bus/at-spi")" = \
  "710:$CONTAINER_TEST_UID:$CONTAINER_TEST_GID"
test "$(stat -c '%a:%u:%g' "$test_root/run/xenoteer/artifacts")" = \
  "700:$CONTAINER_TEST_DAEMON_UID:$CONTAINER_TEST_DAEMON_GID"
test "$(stat -c '%a' "$test_root/home/xenoteer")" = 700
test "$(stat -c '%a' "$test_root/tmp/.X11-unix")" = 1777
test "$(stat -c '%a' "$test_root/tmp/.ICE-unix")" = 1777
test "$(stat -c '%a' "$test_root/run/user/1000/ICEauthority")" = 600
test "$(cat "$test_root/run/xenoteer/env/HOME")" = "$test_root/home/xenoteer"
test "$(cat "$test_root/run/xenoteer/env/XDG_RUNTIME_DIR")" = "$test_root/run/user/1000"
test "$(cat "$test_root/run/xenoteer/env/XDG_CONFIG_HOME")" = "$test_root/run/user/1000/xdg/config"
test "$(cat "$test_root/run/xenoteer/env/XDG_CACHE_HOME")" = "$test_root/run/user/1000/xdg/cache"
test "$(cat "$test_root/run/xenoteer/env/XDG_DATA_HOME")" = "$test_root/run/user/1000/xdg/data"
test "$(cat "$test_root/run/xenoteer/env/DBUS_SESSION_BUS_ADDRESS")" = \
  "unix:path=$test_root/run/xenoteer/bus/session"
test "$(cat "$test_root/run/xenoteer/env/AT_SPI_BUS_ADDRESS")" = \
  "unix:path=$test_root/run/xenoteer/bus/at-spi/bus_77"
test "$(cat "$test_root/run/xenoteer/env/VIEWER_ENABLED")" = 1
test "$(cat "$test_root/run/xenoteer/env/VIEWER_REQUIRED")" = 0
test "$(cat "$test_root/run/xenoteer/env/GTK_OVERLAY_SCROLLING")" = 0
test "$(cat "$test_root/run/xenoteer/env/GDK_SCALE")" = 1
test "$(cat "$test_root/run/xenoteer/env/GDK_DPI_SCALE")" = 1
test "$(cat "$test_root/run/xenoteer/env/QT_AUTO_SCREEN_SCALE_FACTOR")" = 0
test "$(cat "$test_root/run/xenoteer/env/QT_SCALE_FACTOR")" = 1
test "$(cat "$test_root/run/xenoteer/env/QT_FONT_DPI")" = 96
test "$(cat "$test_root/run/xenoteer/env/QT_STYLE_OVERRIDE")" = Fusion
test ! -e "$test_root/run/xenoteer/env/PATH"

viewer_value='viewer-setting-value-canary'
set +e
viewer_output=$(CONTAINER_TEST_ROOT="$test_root" \
  VIEWER_ENABLED="$viewer_value" "$runtime_init" 2>&1)
viewer_status=$?
set -e
test "$viewer_status" -eq 64
grep -Fq 'invalid viewer enabled setting' <<<"$viewer_output"
if grep -Fq "$viewer_value" <<<"$viewer_output"; then
  printf 'invalid viewer setting leaked to diagnostics\n' >&2
  exit 1
fi
if CONTAINER_TEST_ROOT="$test_root" VIEWER_ENABLED=0 VIEWER_REQUIRED=1 \
  "$runtime_init" 2>/dev/null; then
  printf 'runtime initialization accepted a required disabled viewer\n' >&2
  exit 1
fi

token_path_canary='/run/secrets/TOKEN_PATH_CANARY_MUST_NOT_BE_LOGGED'
set +e
token_path_output=$(CONTAINER_TEST_ROOT="$test_root" \
  XENOTEER__AUTH__TOKEN_FILE="$token_path_canary" "$runtime_init" 2>&1)
token_path_status=$?
set -e
test "$token_path_status" -eq 78
grep -Fq 'explicit authentication token file is missing' <<<"$token_path_output"
if grep -Fq "$token_path_canary" <<<"$token_path_output"; then
  printf 'authentication token path leaked to diagnostics\n' >&2
  exit 1
fi

rm -f -- "$test_root/run/secrets/xenoteer_api_token" \
  "$test_root/run/xenoteer/api-token" \
  "$test_root/run/xenoteer/generated-api-token"
generated_output=$(CONTAINER_TEST_ROOT="$test_root" "$runtime_init" 2>&1)
grep -Fq 'generated API bearer token is available to root at /run/xenoteer/generated-api-token' \
  <<<"$generated_output"
test "$(stat -c '%a:%u:%g' "$test_root/run/xenoteer/generated-api-token")" = \
  "400:$CONTAINER_TEST_TOKEN_UID:$CONTAINER_TEST_TOKEN_GID"
test "$(wc -c <"$test_root/run/xenoteer/generated-api-token")" -eq 64
grep -Eq '^[0-9a-f]{64}$' "$test_root/run/xenoteer/generated-api-token"
cmp "$test_root/run/xenoteer/generated-api-token" "$test_root/run/xenoteer/api-token"

invalid_value='single-underscore-value-canary'
set +e
invalid_output=$(CONTAINER_TEST_ROOT="$test_root" \
  XENOTEER_BAD="$invalid_value" "$runtime_init" 2>&1)
invalid_status=$?
set -e
test "$invalid_status" -eq 64
grep -Fq 'invalid Xenoteer environment configuration key' <<<"$invalid_output"
if grep -Fq "$invalid_value" <<<"$invalid_output"; then
  printf 'malformed Xenoteer environment value leaked to diagnostics\n' >&2
  exit 1
fi

malformed_value='malformed-nested-value-canary'
set +e
malformed_output=$(CONTAINER_TEST_ROOT="$test_root" \
  XENOTEER__LOGGING___FILTER="$malformed_value" "$runtime_init" 2>&1)
malformed_status=$?
set -e
test "$malformed_status" -eq 64
grep -Fq 'invalid Xenoteer environment configuration key' <<<"$malformed_output"
if grep -Fq "$malformed_value" <<<"$malformed_output"; then
  printf 'malformed nested Xenoteer environment value leaked to diagnostics\n' >&2
  exit 1
fi

CONTAINER_TEST_ROOT="$test_root" XENOTEER__DESKTOP__DISPLAY_WIDTH='' \
  "$runtime_init"
test "$(wc -c <"$test_root/run/xenoteer/env/XENOTEER__DESKTOP__DISPLAY_WIDTH")" -eq 1
test "$(od -An -tx1 "$test_root/run/xenoteer/env/XENOTEER__DESKTOP__DISPLAY_WIDTH" \
  | tr -d '[:space:]')" = 0a

if CONTAINER_TEST_ROOT="$test_root" XVFB_SCREEN_WIDTH=1280 \
  "$runtime_init" 2>/dev/null; then
  printf 'runtime initialization accepted noncanonical Xvfb geometry\n' >&2
  exit 1
fi

small_shm="$test_root/small-shm"
small_bin="$test_root/small-bin"
mkdir -p "$small_shm" "$small_bin"
chmod 1777 "$small_shm"
cat >"$small_bin/df" <<'MOCK'
#!/bin/sh
printf '1B-blocks\n2147483648\n'
MOCK
chmod 0755 "$small_bin/df"
if PATH="$small_bin:$PATH" CONTAINER_TEST_ROOT="$test_root" \
  SHM_PATH="$small_shm" "$runtime_init" 2>/dev/null; then
  printf 'runtime initialization accepted a 2 GiB /dev/shm\n' >&2
  exit 1
fi

if CONTAINER_TEST_ROOT="$test_root-invalid" DISPLAY='localhost:99' "$runtime_init" 2>/dev/null; then
  printf 'invalid display was accepted\n' >&2
  exit 1
fi

cat >"$mock_bin/xauth" <<'MOCK'
#!/bin/sh
test "$1" = -f
auth_file=$2
: >"$auth_file"
printf '%s\t%s\n' "$auth_file" "$6" >>"$MOCK_XAUTH_LOG"
exit 0
MOCK
chmod 0755 "$mock_bin/xauth"

xauth_log="$test_root/xauth.log"
: >"$xauth_log"
PATH="$mock_bin:$PATH" MOCK_XAUTH_LOG="$xauth_log" \
  CONTAINER_TEST_ROOT="$test_root" "$auth_init"
test "$(stat -c '%a' "$test_root/run/user/1000/Xauthority")" = 600
test "$(stat -c '%a' "$test_root/run/user/1001/Xauthority")" = 600
test "$(wc -l <"$xauth_log")" -eq 2
test "$(cut -f2 "$xauth_log" | uniq | wc -l)" -eq 1
grep -Fq "$test_root/run/user/1000/Xauthority" "$xauth_log"
grep -Fq "$test_root/run/user/1001/Xauthority" "$xauth_log"

cat >"$mock_bin/xdpyinfo" <<'MOCK'
#!/bin/sh
cat <<'OUTPUT'
name of display:    :77
dimensions:    1920x1080 pixels (508x286 millimeters)
resolution:    96x96 dots per inch
depth of root window:    24 planes
XTEST
OUTPUT
MOCK
chmod 0755 "$mock_bin/xdpyinfo"

PATH="$mock_bin:$PATH" \
DISPLAY=:77 \
XAUTHORITY="$test_root/run/user/1000/Xauthority" \
XVFB_SCREEN_WIDTH=1920 \
XVFB_SCREEN_HEIGHT=1080 \
XVFB_SCREEN_DEPTH=24 \
  "$probe_x11"

if PATH="$mock_bin:$PATH" DISPLAY=:77 XAUTHORITY="$test_root/run/user/1000/Xauthority" \
  XVFB_SCREEN_WIDTH=1280 XVFB_SCREEN_HEIGHT=1080 XVFB_SCREEN_DEPTH=24 \
  "$probe_x11" 2>/dev/null; then
  printf 'X11 probe accepted wrong geometry\n' >&2
  exit 1
fi

gdbus_log="$test_root/gdbus.log"
cat >"$mock_bin/gdbus" <<'MOCK'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$MOCK_GDBUS_LOG"
case "$*" in
  *'org.freedesktop.DBus.NameHasOwner'*) printf '(true,)\n' ;;
  *'org.a11y.Bus.GetAddress'*) printf "('%s',)\n" "$AT_SPI_BUS_ADDRESS" ;;
  *'org.freedesktop.DBus.Properties.Get'*) printf '(<true>,)\n' ;;
  *'org.a11y.atspi.Registry.GetRegisteredEvents'*) printf '(@a(ss) [],)\n' ;;
  *) exit 1 ;;
esac
MOCK
chmod 0755 "$mock_bin/gdbus"

: >"$gdbus_log"
PATH="$mock_bin:$PATH" \
MOCK_GDBUS_LOG="$gdbus_log" \
CONTAINER_TEST_ROOT="$test_root" \
CONTAINER_TEST_ALLOW_MISSING_SOCKET=1 \
DBUS_SESSION_BUS_ADDRESS="unix:path=$test_root/run/xenoteer/bus/session" \
  "$probe_session_dbus"
test "$(wc -l <"$gdbus_log")" -eq 1
grep -Fq 'org.freedesktop.DBus.NameHasOwner org.freedesktop.DBus' "$gdbus_log"

: >"$gdbus_log"
PATH="$mock_bin:$PATH" \
MOCK_GDBUS_LOG="$gdbus_log" \
CONTAINER_TEST_ROOT="$test_root" \
CONTAINER_TEST_ALLOW_MISSING_SOCKET=1 \
DISPLAY=:77 \
DBUS_SESSION_BUS_ADDRESS="unix:path=$test_root/run/xenoteer/bus/session" \
AT_SPI_BUS_ADDRESS="unix:path=$test_root/run/xenoteer/bus/at-spi/bus_77" \
  "$probe_atspi"
test "$(wc -l <"$gdbus_log")" -eq 5
sed -n '1p' "$gdbus_log" | grep -Fq \
  'org.freedesktop.DBus.NameHasOwner org.a11y.Bus'
sed -n '2p' "$gdbus_log" | grep -Fq 'org.a11y.Bus.GetAddress'
sed -n '3p' "$gdbus_log" | grep -Fq \
  'org.freedesktop.DBus.Properties.Get org.a11y.Status IsEnabled'
sed -n '4p' "$gdbus_log" | grep -Fq \
  'org.freedesktop.DBus.Properties.Get org.a11y.Status ScreenReaderEnabled'
sed -n '5p' "$gdbus_log" | grep -Fq \
  'org.a11y.atspi.Registry.GetRegisteredEvents'

cat >"$mock_bin/pgrep" <<'MOCK'
#!/bin/sh
set -eu
for argument in "$@"; do process=$argument; done
case "$process" in
  xfce4-session|xfwm4|xfsettingsd|xfdesktop|xfconfd)
    printf '%s\n' "${MOCK_PROCESS_PID:-101}"
    ;;
  xfce4-panel)
    if [ "${DESKTOP_PROFILE:-}" = standard ]; then
      printf '%s\n' "${MOCK_PROCESS_PID:-102}"
    else
      exit 1
    fi
    ;;
  "${MOCK_FORBIDDEN_PROCESS:-__none__}") printf '103\n' ;;
  *) exit 1 ;;
esac
MOCK
cat >"$mock_bin/xprop" <<'MOCK'
#!/bin/sh
set -eu
cat <<OUTPUT
_NET_SUPPORTING_WM_CHECK(WINDOW): window id # 0x200001
_NET_SUPPORTED(ATOM) = _NET_ACTIVE_WINDOW, _NET_NUMBER_OF_DESKTOPS
_NET_NUMBER_OF_DESKTOPS(CARDINAL) = ${MOCK_WORKSPACES:-1}
OUTPUT
MOCK
cat >"$mock_bin/xfconf-query" <<'MOCK'
#!/bin/sh
printf '%s\n' "${MOCK_COMPOSITOR:-false}"
MOCK
cat >"$mock_bin/xset" <<'MOCK'
#!/bin/sh
cat <<'OUTPUT'
Screen Saver:
  prefer blanking:  no    allow exposures:  yes
  timeout:  0    cycle:  0
DPMS (Energy Star):
  DPMS is Disabled
OUTPUT
MOCK
chmod 0755 "$mock_bin/pgrep" "$mock_bin/xprop" "$mock_bin/xfconf-query" \
  "$mock_bin/xset"

for desktop_profile in bare standard; do
  PATH="$mock_bin:$PATH" \
  DISPLAY=:77 \
  XAUTHORITY="$test_root/run/user/1000/Xauthority" \
  DESKTOP_PROFILE="$desktop_profile" \
  MOCK_PROCESS_PID=$$ \
    "$probe_xfce"
done
if PATH="$mock_bin:$PATH" DISPLAY=:77 \
  XAUTHORITY="$test_root/run/user/1000/Xauthority" DESKTOP_PROFILE=bare \
  MOCK_COMPOSITOR=true MOCK_PROCESS_PID=$$ "$probe_xfce" 2>/dev/null; then
  printf 'XFCE probe accepted an enabled compositor\n' >&2
  exit 1
fi
if PATH="$mock_bin:$PATH" DISPLAY=:77 \
  XAUTHORITY="$test_root/run/user/1000/Xauthority" DESKTOP_PROFILE=bare \
  MOCK_WORKSPACES=2 MOCK_PROCESS_PID=$$ "$probe_xfce" 2>/dev/null; then
  printf 'XFCE probe accepted more than one workspace\n' >&2
  exit 1
fi
if PATH="$mock_bin:$PATH" DISPLAY=:77 \
  XAUTHORITY="$test_root/run/user/1000/Xauthority" DESKTOP_PROFILE=bare \
  MOCK_FORBIDDEN_PROCESS=Thunar MOCK_PROCESS_PID=$$ "$probe_xfce" 2>/dev/null; then
  printf 'XFCE probe accepted a forbidden desktop process\n' >&2
  exit 1
fi

cat >"$mock_bin/python3" <<'MOCK'
#!/bin/sh
set -eu
[ "$PYTHONDONTWRITEBYTECODE" = 1 ]
printf '%s\n' "$*" >>"$MOCK_PYTHON_LOG"
MOCK
cat >"$mock_bin/curl" <<'MOCK'
#!/bin/sh
set -eu
case "$*" in
  *'--max-time 2 http://127.0.0.1:6080/vnc.html'*) exit 0 ;;
  *) exit 1 ;;
esac
MOCK
chmod 0755 "$mock_bin/python3" "$mock_bin/curl"
python_log="$test_root/python.log"
: >"$python_log"
PATH="$mock_bin:$PATH" VIEWER_ENABLED=1 MOCK_PYTHON_LOG="$python_log" \
  "$probe_x0tigervnc"
grep -Fq 'socket.create_connection' "$python_log"
PATH="$mock_bin:$PATH" VIEWER_ENABLED=1 MOCK_PYTHON_LOG="$python_log" \
  "$probe_websockify"
if PATH="$mock_bin:$PATH" VIEWER_ENABLED=0 MOCK_PYTHON_LOG="$python_log" \
  "$probe_x0tigervnc" 2>/dev/null; then
  printf 'X0tigervnc probe accepted a disabled viewer\n' >&2
  exit 1
fi

printf 'runtime script tests passed\n'
