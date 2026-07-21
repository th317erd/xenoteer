#!/bin/sh
set -eu

for required in mktemp rm sed; do
    command -v "$required" >/dev/null 2>&1 || {
        echo "missing required command: $required" >&2
        exit 2
    }
done

proof_dir=$(mktemp -d "${TMPDIR:-/tmp}/xenoteer-concurrent-spikes.XXXXXX")
first_pid=
second_pid=
cleanup() {
    if [ -n "$first_pid" ]; then
        kill "$first_pid" 2>/dev/null || true
        wait "$first_pid" 2>/dev/null || true
    fi
    if [ -n "$second_pid" ]; then
        kill "$second_pid" 2>/dev/null || true
        wait "$second_pid" 2>/dev/null || true
    fi
    rm -rf "$proof_dir"
}
trap cleanup EXIT INT TERM

run_pair() {
    pair_name=$1
    pair_script=$2
    "$pair_script" >"$proof_dir/$pair_name-1.log" 2>&1 &
    first_pid=$!
    "$pair_script" >"$proof_dir/$pair_name-2.log" 2>&1 &
    second_pid=$!

    set +e
    wait "$first_pid"
    first_status=$?
    wait "$second_pid"
    second_status=$?
    set -e
    first_pid=
    second_pid=

    if [ "$first_status" -ne 0 ] || [ "$second_status" -ne 0 ]; then
        echo "$pair_name concurrent harness proof failed" >&2
        sed -n '1,240p' "$proof_dir/$pair_name-1.log" >&2
        sed -n '1,240p' "$proof_dir/$pair_name-2.log" >&2
        return 1
    fi
    echo "$pair_name concurrent harness proof passed"
}

run_pair x11 tests/platform/run-x11-spikes.sh
run_pair atspi tests/platform/run-atspi-spike.sh
