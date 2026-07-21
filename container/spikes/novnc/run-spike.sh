#!/bin/sh
# SPDX-License-Identifier: BUSL-1.1
set -eu

runtime=/run/xenoteer-novnc-spike
display=:199
auth_file=$runtime/Xauthority
xvfb_pid=
recorder_pid=
clipboard_pid=
rfb_server_pid=
websockify_pid=
chromium_pid=
succeeded=false

cleanup() {
    if [ "$succeeded" != true ]; then
        for log in xvfb recorder clipboard rfb-server websockify chromium chromium-proof rfb-warmup rfb-probe input-driver; do
            if [ -s "$runtime/$log.log" ]; then
                echo "--- $log.log ---" >&2
                sed -n '1,240p' "$runtime/$log.log" >&2
            fi
        done
    fi
    for process in "$chromium_pid" "$websockify_pid" "$rfb_server_pid" "$clipboard_pid" "$recorder_pid" "$xvfb_pid"; do
        if [ -n "$process" ]; then
            kill "$process" 2>/dev/null || true
        fi
    done
    for process in "$chromium_pid" "$websockify_pid" "$rfb_server_pid" "$clipboard_pid" "$recorder_pid" "$xvfb_pid"; do
        if [ -n "$process" ]; then
            wait "$process" 2>/dev/null || true
        fi
    done
}
trap cleanup EXIT INT TERM

fail() {
    echo "noVNC spike failed: $*" >&2
    exit 1
}

wait_for_pattern() {
    pattern=$1
    file=$2
    process=$3
    attempt=0
    while [ "$attempt" -lt 200 ]; do
        if grep "$pattern" "$file" >/dev/null 2>&1; then
            return 0
        fi
        kill -0 "$process" 2>/dev/null || fail "process $process exited before $pattern"
        attempt=$((attempt + 1))
        sleep 0.05
    done
    fail "timed out waiting for $pattern in $file"
}

assert_uid_1000() {
    process=$1
    name=$2
    uid=$(sed -n 's/^Uid:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "/proc/$process/status")
    [ "$uid" = 1000 ] || fail "$name ran as uid $uid instead of 1000"
}

assert_flag() {
    process=$1
    flag=$2
    tr '\000' '\n' < "/proc/$process/cmdline" | grep -F -x -- "$flag" >/dev/null \
        || fail "process $process is missing mandatory flag $flag"
}

assert_forbidden_flag_absent() {
    process=$1
    flag=$2
    if tr '\000' '\n' < "/proc/$process/cmdline" | grep -F -x -- "$flag" >/dev/null; then
        fail "process $process contains forbidden Chromium flag $flag"
    fi
}

install -d -m 0700 -o 1000 -g 1000 "$runtime"
install -d -m 1777 /tmp/.X11-unix
install -m 0600 -o 1000 -g 1000 /dev/null "$auth_file"
cookie=$(mcookie)
/command/s6-setuidgid xenoteer xauth -f "$auth_file" add "$display" . "$cookie"

DISPLAY=$display XAUTHORITY=$auth_file \
    /command/s6-setuidgid xenoteer Xvfb "$display" \
      -screen 0 800x600x24 -dpi 96 -nolisten tcp -auth "$auth_file" \
      >"$runtime/xvfb.log" 2>&1 &
xvfb_pid=$!
attempt=0
while [ "$attempt" -lt 200 ]; do
    if DISPLAY=$display XAUTHORITY=$auth_file \
        /command/s6-setuidgid xenoteer xdpyinfo >/dev/null 2>&1; then
        break
    fi
    kill -0 "$xvfb_pid" 2>/dev/null || fail "Xvfb exited before X11 readiness"
    attempt=$((attempt + 1))
    sleep 0.05
done
[ "$attempt" -lt 200 ] || fail "Xvfb did not become protocol-ready"
assert_uid_1000 "$xvfb_pid" Xvfb

DISPLAY=$display XAUTHORITY=$auth_file \
    /command/s6-setuidgid xenoteer x11-input-driver \
      --x 700 --y 550 --expected-window 0 \
      >"$runtime/pointer-park.log" 2>&1
DISPLAY=$display XAUTHORITY=$auth_file \
    /command/s6-setuidgid xenoteer x11-event-recorder \
      >"$runtime/recorder.log" 2>&1 &
recorder_pid=$!
wait_for_pattern '"type":"ready"' "$runtime/recorder.log" "$recorder_pid"
recorder_window=$(sed -n \
    's/^{"type":"ready","window":\([0-9][0-9]*\)}$/\1/p' \
    "$runtime/recorder.log")
[ -n "$recorder_window" ] || fail "could not parse recorder window"
assert_uid_1000 "$recorder_pid" x11-event-recorder
DISPLAY=$display XAUTHORITY=$auth_file \
    timeout 5 /command/s6-setuidgid xenoteer xdotool windowfocus --sync "$recorder_window"
wait_for_pattern '"type":"focus_in"' "$runtime/recorder.log" "$recorder_pid"

clipboard_sentinel=phase-0-view-only-clipboard-sentinel
printf '%s' "$clipboard_sentinel" \
    | DISPLAY=$display XAUTHORITY=$auth_file \
      /command/s6-setuidgid xenoteer xclip -quiet -selection clipboard -in \
        >"$runtime/clipboard.log" 2>&1 &
clipboard_pid=$!
attempt=0
while [ "$attempt" -lt 100 ]; do
    clipboard_value=$(DISPLAY=$display XAUTHORITY=$auth_file \
        /command/s6-setuidgid xenoteer xclip -selection clipboard -out 2>/dev/null || true)
    if [ "$clipboard_value" = "$clipboard_sentinel" ]; then
        break
    fi
    kill -0 "$clipboard_pid" 2>/dev/null || fail "clipboard recorder lost ownership early"
    attempt=$((attempt + 1))
    sleep 0.05
done
[ "$clipboard_value" = "$clipboard_sentinel" ] || fail "clipboard sentinel was not installed"
assert_uid_1000 "$clipboard_pid" xclip

DISPLAY=$display XAUTHORITY=$auth_file \
    /command/s6-setuidgid xenoteer X0tigervnc \
      -display "$display" \
      -rfbport 5900 \
      -interface 127.0.0.1 \
      -localhost=1 \
      -SecurityTypes=None \
      -AlwaysShared=1 \
      -DisconnectClients=0 \
      -AcceptKeyEvents=0 \
      -AcceptPointerEvents=0 \
      -AcceptSetDesktopSize=0 \
      -AcceptCutText=0 \
      -SendCutText=0 \
      -MaxCutText=1024 \
      >"$runtime/rfb-server.log" 2>&1 &
rfb_server_pid=$!
/usr/local/libexec/xenoteer/novnc-rfb-probe wait-port 127.0.0.1 5900 --timeout 15
assert_uid_1000 "$rfb_server_pid" X0tigervnc
for flag in -display "$display" -rfbport 5900 -interface 127.0.0.1 \
    -localhost=1 -SecurityTypes=None -AlwaysShared=1 \
    -DisconnectClients=0 -AcceptKeyEvents=0 -AcceptPointerEvents=0 \
    -AcceptSetDesktopSize=0 -AcceptCutText=0 -SendCutText=0 -MaxCutText=1024; do
    assert_flag "$rfb_server_pid" "$flag"
done

/command/s6-setuidgid xenoteer websockify \
    --web=/usr/share/novnc \
    --heartbeat=30 \
    127.0.0.1:6080 \
    127.0.0.1:5900 \
    >"$runtime/websockify.log" 2>&1 &
websockify_pid=$!
/usr/local/libexec/xenoteer/novnc-rfb-probe wait-port 127.0.0.1 6080 --timeout 15
assert_uid_1000 "$websockify_pid" websockify
/usr/local/libexec/xenoteer/novnc-rfb-probe assert-loopback 5900 6080

curl --fail --silent --show-error http://127.0.0.1:6080/vnc.html \
    > "$runtime/served-vnc.html"
curl --fail --silent --show-error http://127.0.0.1:6080/core/rfb.js \
    > "$runtime/served-rfb.js"
curl --fail --silent --show-error http://127.0.0.1:6080/mandatory.json \
    > "$runtime/served-mandatory.json"
cmp /usr/share/novnc/vnc.html "$runtime/served-vnc.html" >/dev/null \
    || fail "served noVNC entry point differs from the pinned asset"
cmp /usr/share/novnc/core/rfb.js "$runtime/served-rfb.js" >/dev/null \
    || fail "served noVNC RFB module differs from the pinned asset"
cmp /usr/share/novnc/mandatory.json "$runtime/served-mandatory.json" >/dev/null \
    || fail "served noVNC mandatory settings differ from the image"
grep '"view_only": true' "$runtime/served-mandatory.json" >/dev/null \
    || fail "noVNC client-side view_only setting is not mandatory"
grep '"view_clip": false' "$runtime/served-mandatory.json" >/dev/null \
    || fail "noVNC viewport clipping setting was not pinned"

if grep -E '"type":"(motion|button_press|button_release|key_press|key_release)"' \
    "$runtime/recorder.log" >/dev/null; then
    fail "recorder observed input before the RFB attempt"
fi

# Force a complete, bounded RFB 3.8 exchange before starting Chromium. Besides
# proving ServerInit/framebuffer delivery independently, this avoids making the
# browser assertion depend on the adapter's first-screen acquisition latency.
/usr/local/libexec/xenoteer/novnc-rfb-probe rfb \
    >"$runtime/rfb-warmup.log" 2>&1
if grep -E '"type":"(motion|button_press|button_release|key_press|key_release)"' \
    "$runtime/recorder.log" >/dev/null; then
    fail "server-side view-only policy allowed warm-up RFB input into X11"
fi
clipboard_value=$(DISPLAY=$display XAUTHORITY=$auth_file \
    /command/s6-setuidgid xenoteer xclip -selection clipboard -out)
[ "$clipboard_value" = "$clipboard_sentinel" ] \
    || fail "server-side view-only policy allowed warm-up ClientCutText into X11"

install -d -m 0700 -o 1000 -g 1000 \
    "$runtime/chromium-home" \
    "$runtime/chromium-profile"
viewer_url='http://127.0.0.1:6080/xenoteer-browser-proof.html'
HOME=$runtime/chromium-home XDG_CONFIG_HOME=$runtime/chromium-home \
    /command/s6-setuidgid xenoteer chromium \
      --headless=new \
      --remote-debugging-address=127.0.0.1 \
      --remote-debugging-port=9222 \
      --disable-gpu \
      --no-default-browser-check \
      --no-first-run \
      --user-data-dir="$runtime/chromium-profile" \
      "$viewer_url" \
      --window-size=1024,768 \
      >"$runtime/chromium.log" 2>&1 &
chromium_pid=$!
/usr/local/libexec/xenoteer/novnc-rfb-probe wait-port 127.0.0.1 9222 --timeout 15
assert_uid_1000 "$chromium_pid" Chromium
assert_forbidden_flag_absent "$chromium_pid" --no-sandbox
assert_forbidden_flag_absent "$chromium_pid" --disable-dev-shm-usage
grep -Eq '^Seccomp:[[:space:]]+2$' "/proc/$chromium_pid/status" \
    || fail "Chromium main process is not using seccomp filter mode"
/usr/local/libexec/xenoteer/novnc-rfb-probe chromium \
    9222 "$runtime/novnc.png" >"$runtime/chromium-proof.log" 2>&1
renderer_count=0
for renderer_pid in $(pgrep -f -- '--type=renderer' || true); do
    assert_uid_1000 "$renderer_pid" Chromium-renderer
    assert_forbidden_flag_absent "$renderer_pid" --no-sandbox
    assert_forbidden_flag_absent "$renderer_pid" --disable-dev-shm-usage
    grep -Eq '^Seccomp:[[:space:]]+2$' "/proc/$renderer_pid/status" \
        || fail "Chromium renderer $renderer_pid is not using seccomp filter mode"
    renderer_count=$((renderer_count + 1))
done
[ "$renderer_count" -gt 0 ] || fail "Chromium created no auditable renderer process"
if grep -E '"type":"(motion|button_press|button_release|key_press|key_release)"' \
    "$runtime/recorder.log" >/dev/null; then
    fail "actual noVNC view-only client emitted X11 input"
fi

/usr/local/libexec/xenoteer/novnc-rfb-probe rfb \
    >"$runtime/rfb-probe.log" 2>&1
sleep 1
kill -0 "$rfb_server_pid" 2>/dev/null || fail "X0tigervnc exited after the RFB probe"
kill -0 "$websockify_pid" 2>/dev/null || fail "websockify exited after the RFB probe"
kill -0 "$chromium_pid" 2>/dev/null || fail "Chromium exited after the noVNC proof"
kill -0 "$recorder_pid" 2>/dev/null || fail "RFB input caused the event recorder to exit"
kill -0 "$clipboard_pid" 2>/dev/null || fail "RFB cut text displaced the X11 clipboard owner"
if grep -E '"type":"(motion|button_press|button_release|key_press|key_release)"' \
    "$runtime/recorder.log" >/dev/null; then
    fail "server-side view-only policy allowed RFB input into X11"
fi
clipboard_value=$(DISPLAY=$display XAUTHORITY=$auth_file \
    /command/s6-setuidgid xenoteer xclip -selection clipboard -out)
[ "$clipboard_value" = "$clipboard_sentinel" ] \
    || fail "server-side view-only policy allowed RFB ClientCutText into X11"

DISPLAY=$display XAUTHORITY=$auth_file \
    /command/s6-setuidgid xenoteer x11-input-driver \
      --x 40 --y 50 --expected-window "$recorder_window" \
      >"$runtime/input-driver.log" 2>&1
wait_for_pattern '"type":"motion".*"root_x":40,"root_y":50' \
    "$runtime/recorder.log" "$recorder_pid"
grep '"type":"motion".*"root_x":40,"root_y":50' "$runtime/recorder.log" >/dev/null \
    || fail "positive-control XTEST motion was not observed"
grep "\"window\":$recorder_window" "$runtime/recorder.log" >/dev/null \
    || fail "positive-control event did not reach the recorder window"
kill "$recorder_pid"
wait "$recorder_pid" 2>/dev/null || true
recorder_pid=

for required in \
    /usr/share/doc/xenoteer-novnc-spike/packages.lock \
    /usr/share/doc/xenoteer-novnc-spike/dpkg-manifest.tsv \
    /usr/share/doc/xenoteer-novnc-spike/direct-license-stanzas.tsv \
    /usr/share/doc/xenoteer-novnc-spike/installed-novnc-assets.sha256 \
    /usr/share/doc/novnc/copyright \
    /usr/share/doc/websockify/copyright \
    /usr/share/doc/tigervnc-scraping-server/copyright \
    /usr/share/doc/xdotool/copyright; do
    [ -s "$required" ] || fail "missing inventory/license evidence: $required"
done
license_inventory=/usr/share/doc/xenoteer-novnc-spike/direct-license-stanzas.tsv
tab=$(printf '\t')
for license in BSD-3-Clause BSD-style-descipher GPL-2+ GPL-3+ LGPL-2.1+ \
    MIT/X11-style fsfap public-domain; do
    grep -F -x "tigervnc-scraping-server${tab}$license" "$license_inventory" >/dev/null \
        || fail "TigerVNC license inventory is missing $license"
done
for license in BSD-2-clause CC-BY-SA-3.0 Expat MPL-2.0 OFL-1.1 Zlib; do
    grep -F -x "novnc${tab}$license" "$license_inventory" >/dev/null \
        || fail "noVNC license inventory is missing $license"
done
for license in BSD-2-clauses GPL-2+ LGPL-2 LGPL-3 MPL-2.0; do
    grep -F -x "websockify${tab}$license" "$license_inventory" >/dev/null \
        || fail "websockify license inventory is missing $license"
done
grep -F -x "xclip${tab}GPL-2.0+" "$license_inventory" >/dev/null \
    || fail "xclip license inventory is missing GPL-2.0+"
grep -F -x "xdotool${tab}BSD-3-clause" "$license_inventory" >/dev/null \
    || fail "xdotool license inventory is missing BSD-3-clause"
grep -F -x \
    "tigervnc-scraping-server${tab}1.15.0+dfsg-2.1~deb13u1${tab}amd64${tab}tigervnc${tab}1.15.0+dfsg-2.1~deb13u1" \
    /usr/share/doc/xenoteer-novnc-spike/dpkg-manifest.tsv >/dev/null \
    || fail "TigerVNC binary-to-source inventory does not match the exact pin"

cat "$runtime/rfb-warmup.log"
cat "$runtime/chromium-proof.log"
cat "$runtime/rfb-probe.log"
novnc_png_sha256=$(sha256sum "$runtime/novnc.png" | awk '{print $1}')
printf '%s\n' "{\"actual_novnc_browser\":true,\"clipboard_unchanged\":true,\"novnc_png_sha256\":\"$novnc_png_sha256\",\"recorder_positive_control\":true,\"server_view_only\":true}"
succeeded=true
