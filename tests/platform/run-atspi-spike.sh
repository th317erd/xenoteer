#!/bin/sh
set -eu

for required in Xvfb xauth xdpyinfo dbus-run-session cargo python3 flock seq chmod mktemp rm gdbus env tr touch sleep cat; do
    command -v "$required" >/dev/null 2>&1 || {
        echo "missing required command: $required" >&2
        exit 2
    }
done

if [ "${XENOTEER_INSIDE_DBUS_SESSION:-0}" != 1 ]; then
    session_runtime=$(mktemp -d "${TMPDIR:-/tmp}/xenoteer-atspi-runtime.XXXXXX")
    chmod 700 "$session_runtime"
    cleanup_outer() {
        rm -rf "$session_runtime"
    }
    trap cleanup_outer EXIT INT TERM
    if XDG_RUNTIME_DIR="$session_runtime" \
        XDG_DATA_DIRS="$(pwd)/fixtures/atspi/share:/usr/local/share:/usr/share" \
        dbus-run-session -- env \
        XDG_RUNTIME_DIR="$session_runtime" \
        XDG_DATA_DIRS="$(pwd)/fixtures/atspi/share:/usr/local/share:/usr/share" \
        XENOTEER_INSIDE_DBUS_SESSION=1 \
        "$0" "$@"; then
        session_status=0
    else
        session_status=$?
    fi
    trap - EXIT INT TERM
    cleanup_outer
    exit "$session_status"
fi

test_dir=$(mktemp -d "${TMPDIR:-/tmp}/xenoteer-atspi-spike.XXXXXX")
xvfb_pid=
fixture_pid=
display_lock_held=false
cleanup() {
    if [ -n "$fixture_pid" ]; then
        kill "$fixture_pid" 2>/dev/null || true
        wait "$fixture_pid" 2>/dev/null || true
    fi
    if [ -n "$xvfb_pid" ]; then
        kill "$xvfb_pid" 2>/dev/null || true
        wait "$xvfb_pid" 2>/dev/null || true
    fi
    if [ "$display_lock_held" = true ]; then
        flock -u 9 2>/dev/null || true
        exec 9>&-
        display_lock_held=false
    fi
    rm -rf "$test_dir"
}
trap cleanup EXIT INT TERM

display_number=
for candidate in $(seq 240 279); do
    display_lock_file=/tmp/xenoteer-x-display-$candidate.lock
    exec 9>"$display_lock_file"
    if flock -n 9; then
        if [ ! -e "/tmp/.X11-unix/X$candidate" ]; then
            display_number=$candidate
            display_lock_held=true
            break
        fi
        flock -u 9
    fi
    exec 9>&-
done
if [ -z "$display_number" ]; then
    echo "no free isolated X display number in 240..279" >&2
    exit 2
fi

display=:$display_number
echo "allocated AT-SPI spike display $display"
auth_file=$test_dir/Xauthority
cookie=$(tr -d '-' </proc/sys/kernel/random/uuid)
touch "$auth_file"
xauth -f "$auth_file" add "$display" . "$cookie"
Xvfb "$display" -screen 0 800x600x24 -dpi 96 -nolisten tcp -auth "$auth_file" \
    >"$test_dir/xvfb.log" 2>&1 &
xvfb_pid=$!

export DISPLAY="$display"
export XAUTHORITY="$auth_file"
export NO_AT_BRIDGE=0
export GTK_A11Y=atk-bridge
export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1

attempt=0
while ! xdpyinfo >/dev/null 2>&1; do
    kill -0 "$xvfb_pid" 2>/dev/null || {
        cat "$test_dir/xvfb.log" >&2
        exit 1
    }
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ]; then
        echo "Xvfb failed protocol readiness" >&2
        exit 1
    fi
    sleep 0.02
done

python3 fixtures/atspi/minimal_gtk.py --ready-file "$test_dir/fixture.ready" \
    >"$test_dir/fixture.log" 2>&1 &
fixture_pid=$!
attempt=0
while [ ! -s "$test_dir/fixture.ready" ]; do
    kill -0 "$fixture_pid" 2>/dev/null || {
        cat "$test_dir/fixture.log" >&2
        exit 1
    }
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ]; then
        echo "GTK fixture failed readiness" >&2
        cat "$test_dir/fixture.log" >&2
        exit 1
    fi
    sleep 0.02
done

accessibility_bus_address=$(gdbus call --session \
    --dest org.a11y.Bus \
    --object-path /org/a11y/bus \
    --method org.a11y.Bus.GetAddress)
echo "AT-SPI runtime $XDG_RUNTIME_DIR address $accessibility_bus_address"

XENOTEER_ATSPI_FIXTURE_PID=$fixture_pid \
    cargo test -p xenoteer-atspi --all-features --test probe -- --ignored --test-threads=1
