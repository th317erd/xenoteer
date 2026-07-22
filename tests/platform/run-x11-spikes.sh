#!/bin/sh
set -eu

for required in Xvfb xauth xdpyinfo cargo flock seq mktemp rm tr touch grep sed sleep cat; do
    command -v "$required" >/dev/null 2>&1 || {
        echo "missing required command: $required" >&2
        exit 2
    }
done

test_dir=$(mktemp -d "${TMPDIR:-/tmp}/xenoteer-x11-spike.XXXXXX")
xvfb_pid=
recorder_pid=
display_lock_held=false
cleanup() {
    if [ -n "$recorder_pid" ]; then
        kill "$recorder_pid" 2>/dev/null || true
        wait "$recorder_pid" 2>/dev/null || true
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
for candidate in $(seq 180 239); do
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
    echo "no free isolated X display number in 180..239" >&2
    exit 2
fi

display=:$display_number
echo "allocated X11 spike display $display"
auth_file=$test_dir/Xauthority
cookie=$(tr -d '-' </proc/sys/kernel/random/uuid)
touch "$auth_file"
xauth -f "$auth_file" add "$display" . "$cookie"
Xvfb "$display" -screen 0 800x600x24 -dpi 96 -nolisten tcp -auth "$auth_file" \
    >"$test_dir/xvfb.log" 2>&1 &
xvfb_pid=$!

ready=false
attempt=0
while [ "$attempt" -lt 100 ]; do
    if DISPLAY=$display XAUTHORITY=$auth_file xdpyinfo >/dev/null 2>&1; then
        ready=true
        break
    fi
    kill -0 "$xvfb_pid" 2>/dev/null || {
        cat "$test_dir/xvfb.log" >&2
        exit 1
    }
    attempt=$((attempt + 1))
    sleep 0.02
done
if [ "$ready" != true ]; then
    echo "Xvfb failed protocol readiness" >&2
    cat "$test_dir/xvfb.log" >&2
    exit 1
fi

export DISPLAY="$display"
export XAUTHORITY="$auth_file"
cargo test -j 4 -p xenoteer-x11 --all-features --test x11_live -- --ignored --test-threads=1
cargo build -j 4 --manifest-path fixtures/x11/Cargo.toml
fixtures/x11/target/debug/x11-color-bars --exit-after-expose >"$test_dir/color-bars.jsonl"
# Keep the recorder connection (and therefore its window) alive until the
# independent driver has completed its QueryPointer assertion.  Letting the
# recorder exit on MotionNotify races the driver's reply handling: XTEST can
# deliver and record the motion, then the recorder can destroy its window
# before the driver validates which child was under the pointer.
fixtures/x11/target/debug/x11-event-recorder >"$test_dir/recorder.jsonl" &
recorder_pid=$!

grep '"type":"ready"' "$test_dir/color-bars.jsonl" >/dev/null || {
    echo "color-bar fixture did not report readiness" >&2
    exit 1
}

recorder_ready=false
attempt=0
while [ "$attempt" -lt 100 ]; do
    if grep '"type":"ready"' "$test_dir/recorder.jsonl" >/dev/null 2>&1; then
        recorder_ready=true
        break
    fi
    kill -0 "$recorder_pid" 2>/dev/null || {
        wait "$recorder_pid" || true
        echo "event-recorder exited before readiness" >&2
        cat "$test_dir/recorder.jsonl" >&2
        exit 1
    }
    attempt=$((attempt + 1))
    sleep 0.02
done
if [ "$recorder_ready" != true ]; then
    echo "event-recorder fixture did not report readiness" >&2
    exit 1
fi

recorder_window=$(sed -n 's/^{"type":"ready","window":\([0-9][0-9]*\)}$/\1/p' \
    "$test_dir/recorder.jsonl")
if [ -z "$recorder_window" ]; then
    echo "could not parse recorder window from Ready record" >&2
    cat "$test_dir/recorder.jsonl" >&2
    exit 1
fi

if ! fixtures/x11/target/debug/x11-input-driver \
    --x 40 --y 50 --expected-window "$recorder_window" \
    >"$test_dir/input-driver.jsonl"; then
    echo "independent XTEST driver failed" >&2
    cat "$test_dir/recorder.jsonl" >&2
    exit 1
fi

motion_observed=false
attempt=0
while [ "$attempt" -lt 100 ]; do
    if grep '"type":"motion".*"root_x":40,"root_y":50' \
        "$test_dir/recorder.jsonl" >/dev/null 2>&1; then
        motion_observed=true
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.02
done
if [ "$motion_observed" != true ]; then
    echo "event-recorder did not observe XTEST motion before timeout" >&2
    cat "$test_dir/recorder.jsonl" >&2
    exit 1
fi
# The Motion record is flushed synchronously. The harness owns recorder
# teardown so the proof window outlives every assertion made by the driver.
kill "$recorder_pid"
wait "$recorder_pid" 2>/dev/null || true
recorder_pid=

grep '"type":"motion".*"root_x":40,"root_y":50' "$test_dir/recorder.jsonl" >/dev/null || {
    echo "event-recorder did not record the expected XTEST MotionNotify" >&2
    cat "$test_dir/recorder.jsonl" >&2
    exit 1
}
grep "\"type\":\"motion\",\"window\":$recorder_window" "$test_dir/recorder.jsonl" >/dev/null || {
    echo "MotionNotify was not delivered to the Ready recorder window" >&2
    cat "$test_dir/recorder.jsonl" >&2
    exit 1
}
grep "\"type\":\"query_pointer_barrier\",\"root_x\":40,\"root_y\":50,\"child\":$recorder_window" \
    "$test_dir/input-driver.jsonl" >/dev/null || {
    echo "same-connection QueryPointer barrier evidence is absent" >&2
    cat "$test_dir/input-driver.jsonl" >&2
    exit 1
}
