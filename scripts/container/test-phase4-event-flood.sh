#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -Eeuo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

# This focused boundary is intentionally local to the event-flood runner. The
# sibling runners have different build layouts; sharing their partially similar
# privilege logic would broaden this acceptance fix without one common contract.
host_rust_directory_is_trusted() {
  local path=$1 uid=$2 gid=$3 canonical component owner group mode permissions
  local -a components
  if [[ $path != /* || $path == *$'\n'* || ! -d $path || -L $path ]]; then
    return 1
  fi
  canonical=$(readlink -e -- "$path") || return 1
  if [[ $canonical != "$path" ]]; then
    return 1
  fi
  IFS=/ read -r -a components <<<"${path#/}"
  component=/
  for path in "${components[@]}"; do
    [[ -n $path ]] || continue
    if [[ $component == / ]]; then
      component="/$path"
    else
      component="$component/$path"
    fi
    read -r owner group mode < <(stat -Lc '%u %g %a' -- "$component") \
      || return 1
    if [[ ! $owner =~ ^[0-9]+$ || ! $group =~ ^[0-9]+$ \
      || ! $mode =~ ^[0-7]{3,4}$ ]]; then
      return 1
    fi
    permissions=$((8#$mode))
    if ((owner != 0 && owner != uid)); then
      return 1
    fi
    if ((permissions & 0002)); then
      return 1
    fi
    if ((permissions & 0020)) \
      && ((owner != uid || group != gid)); then
      return 1
    fi
    if ((owner == uid)); then
      ((permissions & 0100)) || return 1
    elif ((group == gid)); then
      ((permissions & 0010)) || return 1
    else
      ((permissions & 0001)) || return 1
    fi
  done
}

host_rust_user_tool_is_trusted() {
  local path=$1 cargo_bin=$2 uid=$3 gid=$4
  local canonical owner group mode permissions link_owner
  if [[ $path != "$cargo_bin/cargo" && $path != "$cargo_bin/rustc" ]]; then
    return 1
  fi
  host_rust_directory_is_trusted "$cargo_bin" "$uid" "$gid" || return 1
  if [[ ! -e $path || ! -x $path ]]; then
    return 1
  fi
  link_owner=$(stat -c '%u' -- "$path") || return 1
  if [[ ! $link_owner =~ ^[0-9]+$ || $link_owner -ne $uid ]]; then
    return 1
  fi
  canonical=$(readlink -e -- "$path") || return 1
  if [[ ${canonical%/*} != "$cargo_bin" || ! -f $canonical || ! -x $canonical ]]; then
    return 1
  fi
  read -r owner group mode < <(stat -Lc '%u %g %a' -- "$canonical") \
    || return 1
  if [[ ! $owner =~ ^[0-9]+$ || ! $group =~ ^[0-9]+$ \
    || ! $mode =~ ^[0-7]{3,4}$ || $owner -ne $uid ]]; then
    return 1
  fi
  permissions=$((8#$mode))
  if ((permissions & 0022)) || ! ((permissions & 0100)); then
    return 1
  fi
}

host_rust_ambient_tool_is_trusted() {
  local path=$1 uid=$2 gid=$3 canonical owner group mode permissions link_owner
  if [[ $path != /* || $path == *$'\n'* || ! -e $path || ! -x $path ]]; then
    return 1
  fi
  host_rust_directory_is_trusted "${path%/*}" "$uid" "$gid" || return 1
  link_owner=$(stat -c '%u' -- "$path") || return 1
  if [[ ! $link_owner =~ ^[0-9]+$ || ( $link_owner -ne 0 && $link_owner -ne $uid ) ]]; then
    return 1
  fi
  canonical=$(readlink -e -- "$path") || return 1
  host_rust_directory_is_trusted "${canonical%/*}" "$uid" "$gid" || return 1
  if [[ ! -f $canonical || ! -x $canonical ]]; then
    return 1
  fi
  read -r owner group mode < <(stat -Lc '%u %g %a' -- "$canonical") \
    || return 1
  if [[ ! $owner =~ ^[0-9]+$ || ! $group =~ ^[0-9]+$ \
    || ! $mode =~ ^[0-7]{3,4}$ \
    || ( $owner -ne 0 && $owner -ne $uid ) ]]; then
    return 1
  fi
  permissions=$((8#$mode))
  if ((permissions & 0022)); then
    return 1
  fi
  if ((owner == uid)); then
    ((permissions & 0100)) || return 1
  elif ((group == gid)); then
    ((permissions & 0010)) || return 1
  else
    ((permissions & 0001)) || return 1
  fi
}

host_rust_toolchain_resolve() {
  local current_uid passwd_record account_uid account_gid account_home
  local cargo_bin cargo_output env_binary host_line rustc_output
  local -a passwd_fields rust_host_lines
  local sudo_context=0
  current_uid=$(id -u)
  if [[ ! $current_uid =~ ^(0|[1-9][0-9]{0,9})$ ]]; then
    printf 'current UID was invalid\n' >&2
    return 77
  fi
  if [[ ${SUDO_UID+x} ]]; then
    sudo_context=1
    invoking_uid=$SUDO_UID
  else
    invoking_uid=$current_uid
  fi
  if [[ ! $invoking_uid =~ ^(0|[1-9][0-9]{0,9})$ ]] \
    || ((10#$invoking_uid > 4294967295)); then
    printf 'invoking UID was invalid\n' >&2
    return 77
  fi
  if ((current_uid != 0 && invoking_uid != current_uid)); then
    printf 'only root may select a different invoking UID\n' >&2
    return 77
  fi
  if ! passwd_record=$(getent passwd "$invoking_uid") || [[ -z $passwd_record ]] \
    || [[ $passwd_record == *$'\n'* ]]; then
    printf 'could not resolve the invoking user account\n' >&2
    return 77
  fi
  IFS=: read -r -a passwd_fields <<<"$passwd_record"
  if ((${#passwd_fields[@]} != 7)); then
    printf 'invoking user account was malformed\n' >&2
    return 77
  fi
  account_uid=${passwd_fields[2]}
  account_gid=${passwd_fields[3]}
  account_home=${passwd_fields[5]}
  if [[ ! $account_uid =~ ^(0|[1-9][0-9]{0,9})$ \
    || ! $account_gid =~ ^(0|[1-9][0-9]{0,9})$ \
    || $account_uid -ne $invoking_uid ]]; then
    printf 'invoking user account was malformed\n' >&2
    return 77
  fi
  invoking_gid=$account_gid
  if [[ $account_home != /* || ! -d $account_home || -L $account_home ]] \
    || ! host_rust_directory_is_trusted \
      "$account_home" "$invoking_uid" "$invoking_gid"; then
    printf 'invoking user home is unavailable or untrusted\n' >&2
    return 77
  fi
  invoking_home=$account_home
  cargo_bin="$invoking_home/.cargo/bin"
  cargo_binary="$cargo_bin/cargo"
  rustc_binary="$cargo_bin/rustc"
  if [[ -e $cargo_binary || -e $rustc_binary ]]; then
    if [[ ! -x $cargo_binary || ! -x $rustc_binary ]]; then
      printf 'invoking user Rust toolchain is unavailable\n' >&2
      return 77
    fi
    if ! host_rust_user_tool_is_trusted \
      "$cargo_binary" "$cargo_bin" "$invoking_uid" "$invoking_gid" \
      || ! host_rust_user_tool_is_trusted \
        "$rustc_binary" "$cargo_bin" "$invoking_uid" "$invoking_gid"; then
      printf 'invoking user Rust toolchain is untrusted\n' >&2
      return 77
    fi
  elif ((sudo_context)); then
    printf 'invoking user Rust toolchain is unavailable\n' >&2
    return 77
  else
    cargo_binary=$(command -v cargo || true)
    rustc_binary=$(command -v rustc || true)
    if [[ -z $cargo_binary || -z $rustc_binary ]]; then
      printf 'Rust toolchain is unavailable\n' >&2
      return 77
    fi
    if ! host_rust_ambient_tool_is_trusted \
      "$cargo_binary" "$invoking_uid" "$invoking_gid" \
      || ! host_rust_ambient_tool_is_trusted \
        "$rustc_binary" "$invoking_uid" "$invoking_gid"; then
      printf 'ambient Rust toolchain path is untrusted\n' >&2
      return 77
    fi
  fi
  env_binary=$(command -v env || true)
  if [[ -z $env_binary ]] \
    || ! host_rust_ambient_tool_is_trusted \
      "$env_binary" "$current_uid" 0; then
    printf 'trusted env is unavailable for the invoking user boundary\n' >&2
    return 77
  fi
  rust_identity_runner=()
  if ((invoking_uid != current_uid)); then
    sudo_binary=$(command -v sudo || true)
    if [[ -z $sudo_binary ]] \
      || ! host_rust_ambient_tool_is_trusted \
        "$sudo_binary" "$current_uid" 0; then
      printf 'trusted sudo is unavailable for the invoking user boundary\n' >&2
      return 77
    fi
    rust_identity_runner=("$sudo_binary" -H -u "#$invoking_uid")
  fi
  rust_identity_runner+=(
    "$env_binary" -i
    "HOME=$invoking_home"
    "CARGO_HOME=$invoking_home/.cargo"
    "RUSTUP_HOME=$invoking_home/.rustup"
    "RUSTC=$rustc_binary"
    "PATH=$cargo_bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    "LANG=C.UTF-8"
    "LC_ALL=C.UTF-8"
  )
  if ! rustc_output=$("${rust_identity_runner[@]}" "$rustc_binary" -vV); then
    printf 'rustc failed for the invoking user\n' >&2
    return 77
  fi
  if ! cargo_output=$("${rust_identity_runner[@]}" "$cargo_binary" --version) \
    || [[ -z $cargo_output ]]; then
    printf 'cargo failed for the invoking user\n' >&2
    return 77
  fi
  rust_host_lines=()
  while IFS= read -r host_line; do
    if [[ $host_line == host:* ]]; then
      rust_host_lines+=("$host_line")
    fi
  done <<<"$rustc_output"
  if ((${#rust_host_lines[@]} != 1)) \
    || [[ ! ${rust_host_lines[0]} =~ ^host:\ ([a-z0-9_]+(-[a-z0-9_]+)+)$ ]] \
    || [[ ${BASH_REMATCH[1]:-} != *-linux-* ]]; then
    printf 'rustc must report exactly one path-safe Linux host target\n' >&2
    return 77
  fi
  rust_host=${BASH_REMATCH[1]}
}

if [[ ${1:-} == --self-test-host-rust-toolchain ]]; then
  if [[ $# -ne 1 ]]; then
    printf 'usage: test-phase4-event-flood.sh --self-test-host-rust-toolchain\n' >&2
    exit 64
  fi
  for command in env getent id readlink stat; do
    if ! command -v "$command" >/dev/null 2>&1; then
      printf 'required command is unavailable: %s\n' "$command" >&2
      exit 77
    fi
  done
  host_rust_toolchain_resolve
  printf 'host=%s uid=%s\n' "$rust_host" "$invoking_uid"
  exit 0
fi

if [[ $# -ne 1 || -z $1 ]]; then
  printf 'usage: test-phase4-event-flood.sh IMAGE\n' >&2
  exit 64
fi
image_reference=$1
container_name="xenoteer-phase4-event-flood-$$"
tmp_parent=${TMPDIR:-/tmp}
if [[ $tmp_parent != /* || ! -d $tmp_parent ]]; then
  printf 'TMPDIR must name an existing absolute directory\n' >&2
  exit 73
fi
tmp_parent=$(cd "$tmp_parent" && pwd -P)
if [[ $tmp_parent == / ]]; then
  test_template=/xenoteer-phase4-event-flood.XXXXXXXXXX
else
  test_template="$tmp_parent/xenoteer-phase4-event-flood.XXXXXXXXXX"
fi
test_dir=$(mktemp -d -- "$test_template")
test_dir_parent=$(cd "${test_dir%/*}" && pwd -P)
test_dir_name=${test_dir##*/}
if [[ $test_dir_parent != "$tmp_parent" \
  || ! $test_dir_name =~ ^xenoteer-phase4-event-flood\.[A-Za-z0-9]{10}$ \
  || ! -d $test_dir || -L $test_dir ]]; then
  printf 'mktemp returned an unexpected test directory\n' >&2
  rmdir -- "$test_dir" 2>/dev/null || true
  exit 73
fi
test_dir_valid=1
token_file="$test_dir/api-token"
curl_auth_config="$test_dir/curl-auth.conf"
container_created=0
token_canary='PHASE4_EVENT_FLOOD_TOKEN_MUST_NEVER_APPEAR_IN_LOGS_0123456789'

safe_container_logs() {
  local log_file="$test_dir/container.log"
  if [[ $container_created -ne 1 ]]; then
    return
  fi
  timeout --signal=TERM --kill-after=5s 15s docker logs "$container_name" \
    >"$log_file" 2>&1 || return
  if grep -Fq -- "$token_canary" "$log_file"; then
    printf 'container logs suppressed because they contain the API-token canary\n' >&2
    return
  fi
  printf '%s\n' '--- sanitized Phase 4 event-flood container logs ---' >&2
  sed -n '1,240p' "$log_file" >&2
}

report_error() {
  local status=$1
  trap - ERR
  printf 'Phase 4 event-flood gate failed at line %s (status %s)\n' \
    "${BASH_LINENO[0]:-unknown}" "$status" >&2
  safe_container_logs
  exit "$status"
}

cleanup() {
  trap - ERR
  if [[ $container_created -eq 1 ]]; then
    timeout --signal=TERM --kill-after=5s 20s docker rm --force --volumes \
      "$container_name" >/dev/null 2>&1 || true
  fi
  if [[ $test_dir_valid -eq 1 ]]; then
    rm -rf -- "$test_dir"
  fi
}
trap 'report_error $?' ERR
trap cleanup EXIT

for command in curl docker env flock getent id ionice jq mktemp nice \
  python3 readlink stat timeout; do
  if ! command -v "$command" >/dev/null 2>&1; then
    printf 'required command is unavailable: %s\n' "$command" >&2
    exit 77
  fi
done

image=$(timeout --signal=TERM --kill-after=5s 20s docker image inspect \
  "$image_reference" --format '{{.Id}}')
if [[ ! $image =~ ^sha256:[0-9a-f]{64}$ ]]; then
  printf 'resolved image has an invalid immutable ID: %s\n' "$image" >&2
  exit 1
fi

host_rust_toolchain_resolve
rust_arch=${rust_host%%-*}
case "$rust_arch" in
  x86_64) expected_arch=amd64 ;;
  aarch64) expected_arch=arm64 ;;
  armv7*) expected_arch=arm ;;
  i?86) expected_arch=386 ;;
  *)
    printf 'unsupported fixture Rust host architecture: %s\n' "$rust_host" >&2
    exit 77
    ;;
esac
image_os=$(timeout --signal=TERM --kill-after=5s 20s docker image inspect \
  "$image" --format '{{.Os}}')
image_arch=$(timeout --signal=TERM --kill-after=5s 20s docker image inspect \
  "$image" --format '{{.Architecture}}')
if [[ $image_os != linux || $image_arch != "$expected_arch" ]]; then
  printf 'fixture/image platform mismatch: Rust %s, image %s/%s\n' \
    "$rust_host" "$image_os" "$image_arch" >&2
  exit 77
fi

install -d -m 1777 /tmp/codex
build_lock=/tmp/codex/xenoteer-heavy-build.lock
cargo_args=(
  build --quiet --release --locked --jobs 2
  --manifest-path "$repo_root/fixtures/x11/Cargo.toml"
  --bin x11-window-churn
  --target "$rust_host"
)
timeout --signal=TERM --kill-after=30s 300s flock "$build_lock" \
  "${rust_identity_runner[@]}" nice -n 15 ionice -c 3 \
  "$cargo_binary" "${cargo_args[@]}"
fixture_binary="$repo_root/fixtures/x11/target/$rust_host/release/x11-window-churn"
if [[ ! -x $fixture_binary ]]; then
  printf 'fixture build did not produce x11-window-churn\n' >&2
  exit 1
fi

printf '%s' "$token_canary" >"$token_file"
chmod 0400 "$token_file"
if [[ $(id -u) -eq 0 ]]; then
  chown 0:0 "$token_file"
elif [[ $(timeout --signal=TERM --kill-after=5s 20s \
  docker info --format '{{json .SecurityOptions}}') == *'name=rootless'* ]]; then
  : # Rootless Docker maps this owner to container UID zero.
else
  printf 'run as root or use rootless Docker for the root-owned token mount\n' >&2
  exit 77
fi
printf 'header = "Authorization: Bearer %s"\n' "$token_canary" >"$curl_auth_config"
chmod 0600 "$curl_auth_config"

daemon_mount=()
if [[ -n ${XENOTEERD_BINARY_OVERRIDE:-} ]]; then
  if [[ $XENOTEERD_BINARY_OVERRIDE != /* || ! -f $XENOTEERD_BINARY_OVERRIDE \
    || ! -x $XENOTEERD_BINARY_OVERRIDE ]]; then
    printf 'XENOTEERD_BINARY_OVERRIDE must name an absolute executable file\n' >&2
    exit 64
  fi
  daemon_mount=(--volume "$XENOTEERD_BINARY_OVERRIDE:/usr/local/bin/xenoteerd:ro")
fi

timeout --signal=TERM --kill-after=10s 40s docker run --detach \
  --name "$container_name" \
  --cpus 2 \
  --memory 6g \
  --pids-limit 512 \
  --shm-size 4g \
  --log-driver json-file \
  --log-opt max-size=2m \
  --log-opt max-file=1 \
  --publish '127.0.0.1::8080' \
  --volume "$token_file:/run/secrets/xenoteer_api_token:ro" \
  "${daemon_mount[@]}" \
  "$image" >/dev/null
container_created=1

port_binding=$(timeout --signal=TERM --kill-after=5s 15s \
  docker port "$container_name" 8080/tcp)
if [[ ! $port_binding =~ ^127\.0\.0\.1:([0-9]{1,5})$ ]]; then
  printf 'Docker returned an unexpected API binding: %s\n' "$port_binding" >&2
  exit 1
fi
host_port=${BASH_REMATCH[1]}
api_base="http://127.0.0.1:$host_port"

ready=0
for _ in {1..90}; do
  if [[ $(curl --silent --output /dev/null --connect-timeout 1 --max-time 2 \
    --max-filesize 1048576 --write-out '%{http_code}' \
    "$api_base/readyz" || true) == 200 ]]; then
    ready=1
    break
  fi
  if [[ $(timeout --signal=TERM --kill-after=5s 15s docker inspect \
    "$container_name" --format '{{.State.Running}}') != true ]]; then
    printf 'event-flood container stopped before readiness\n' >&2
    exit 1
  fi
  sleep 1
done
if [[ $ready -ne 1 ]]; then
  printf 'event-flood container did not become ready\n' >&2
  exit 1
fi

status_body="$test_dir/status.json"
status_code=$(curl --config "$curl_auth_config" --silent --show-error \
  --connect-timeout 3 --max-time 8 --max-filesize 1048576 \
  --output "$status_body" --write-out '%{http_code}' \
  --header 'Accept: application/json' "$api_base/v1/status")
if [[ $status_code != 200 ]]; then
  printf 'authenticated status returned HTTP %s\n' "$status_code" >&2
  exit 1
fi
desktop_id=$(jq -er '.desktop.id' "$status_body")
desktop_generation=$(jq -er '.desktop.generation' "$status_body")
if [[ ! $desktop_id =~ ^[0-9a-f-]{36}$ || ! $desktop_generation =~ ^[0-9a-f-]{36}$ ]]; then
  printf 'status returned an invalid desktop identity\n' >&2
  exit 1
fi

fixture_dir=/run/xenoteer/phase4-event-flood
timeout --signal=TERM --kill-after=5s 15s docker exec "$container_name" \
  install -d -o 0 -g 0 -m 0755 "$fixture_dir"
timeout --signal=TERM --kill-after=5s 30s docker cp "$fixture_binary" \
  "$container_name:$fixture_dir/x11-window-churn" >/dev/null
timeout --signal=TERM --kill-after=5s 15s docker exec "$container_name" \
  chown 0:0 "$fixture_dir/x11-window-churn"
timeout --signal=TERM --kill-after=5s 15s docker exec "$container_name" \
  chmod 0555 "$fixture_dir/x11-window-churn"
linkage_output=$(timeout --signal=TERM --kill-after=5s 15s docker exec \
  "$container_name" ldd "$fixture_dir/x11-window-churn" 2>&1 || true)
if grep -Eqi 'not found|version [`'"'"'][^`'"'"']+[`'"'"'] not found' \
  <<<"$linkage_output"; then
  printf 'x11-window-churn is ABI-incompatible with the tested image\n' >&2
  exit 1
fi

nice -n 15 ionice -c 3 timeout --signal=TERM --kill-after=15s 120s \
  env PYTHONDONTWRITEBYTECODE=1 \
  "$repo_root/scripts/container/test-phase4-event-flood.py" \
  --api-base "$api_base" \
  --token-file "$token_file" \
  --desktop-id "$desktop_id" \
  --desktop-generation "$desktop_generation" \
  --container-name "$container_name"

timeout --signal=TERM --kill-after=10s 60s docker stop --time 40 \
  "$container_name" >/dev/null
container_exit=$(timeout --signal=TERM --kill-after=5s 15s docker inspect \
  "$container_name" --format '{{.State.ExitCode}}')
if [[ $container_exit -ne 0 ]]; then
  printf 'event-flood container returned exit code %s\n' "$container_exit" >&2
  exit 1
fi
timeout --signal=TERM --kill-after=5s 15s docker logs "$container_name" \
  >"$test_dir/final-container.log" 2>&1
if grep -Fq -- "$token_canary" "$test_dir/final-container.log"; then
  printf 'container logs exposed the API-token canary\n' >&2
  exit 1
fi

printf 'Phase 4 event-flood gate passed: %s (%s)\n' "$image_reference" "$image"
