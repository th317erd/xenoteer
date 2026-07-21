#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
runtime_init="$repo_root/container/rootfs/usr/local/libexec/xenoteer/init-runtime"
auth_init="$repo_root/container/rootfs/usr/local/libexec/xenoteer/init-xauthority"
probe_x11="$repo_root/container/rootfs/usr/local/libexec/xenoteer/probe-x11"
test_root=$(mktemp -d)
mock_bin=$(mktemp -d)
trap 'rm -rf -- "$test_root" "$mock_bin"' EXIT

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
  $'XENOTEER__AUTH__TOKEN_FILE\nXENOTEER__FUTURE_SECTION__FUTURE_FIELD\nXENOTEER__LOGGING__FILTER\nXENOTEER__SERVER__LISTEN'
test "$(stat -c '%a' "$test_root/run/user/1000")" = 700
test "$(stat -c '%a' "$test_root/home/xenoteer")" = 700
test "$(stat -c '%a' "$test_root/tmp/.X11-unix")" = 1777
test ! -e "$test_root/run/xenoteer/env/PATH"

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
exit 0
MOCK
chmod 0755 "$mock_bin/xauth"

PATH="$mock_bin:$PATH" CONTAINER_TEST_ROOT="$test_root" "$auth_init"
test "$(stat -c '%a' "$test_root/run/user/1000/Xauthority")" = 600

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

printf 'runtime script tests passed\n'
