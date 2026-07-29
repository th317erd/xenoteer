#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -Eeuo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
self_test_mode=0
case ${1:-} in
  --self-test-err-trap)
    self_test_mode=1
    image_reference=__err_trap_self_test__
    ;;
  --self-test-err-trap-child)
    self_test_mode=2
    image_reference=__err_trap_self_test__
    ;;
  '')
    printf 'usage: test-phase3-control-plane.sh IMAGE\n' >&2
    exit 64
    ;;
  *) image_reference=$1 ;;
esac
container_name="xenoteer-phase3-control-plane-$$"
restricted_container_name="xenoteer-phase3-restricted-grants-$$"
test_dir=$(mktemp -d)
token_file="$test_dir/api-token"
curl_auth_config="$test_dir/curl-auth.conf"
container_created=0
restricted_container_created=0
fixture_rust_target=
token_canary='PHASE3_CONTROL_PLANE_TOKEN_MUST_NEVER_APPEAR_IN_LOGS_0123456789'

safe_container_logs() {
  local name created label log_file
  while IFS='|' read -r name created label; do
    if [[ $created -ne 1 ]]; then
      continue
    fi
    log_file="$test_dir/$label-container.log"
    docker logs "$name" >"$log_file" 2>&1 || continue
    if grep -Fq -- "$token_canary" "$log_file"; then
      printf '%s logs suppressed because they contain the API-token canary\n' \
        "$label" >&2
      continue
    fi
    printf '%s\n' "--- sanitized $label container logs ---" >&2
    sed -n '1,240p' "$log_file" >&2
  done <<EOF
$container_name|$container_created|primary
$restricted_container_name|$restricted_container_created|restricted
EOF
}

report_error() {
  local status=$1
  trap - ERR
  printf 'Phase 3 control-plane acceptance failed at line %s (status %s)\n' \
    "${BASH_LINENO[0]:-unknown}" "$status" >&2
  safe_container_logs
  exit "$status"
}

cleanup() {
  trap - ERR
  if [[ $restricted_container_created -eq 1 ]]; then
    docker rm --force --volumes "$restricted_container_name" >/dev/null 2>&1 || true
  fi
  if [[ $container_created -eq 1 ]]; then
    docker exec "$container_name" pkill -TERM -u 1000 -f \
      '^/run/xenoteer/phase3-control-plane/x11-event-recorder --focus-before-ready --max-events 512$' \
      >/dev/null 2>&1 || true
    docker rm --force --volumes "$container_name" >/dev/null 2>&1 || true
  fi
  rm -rf -- "$test_dir"
}
trap 'report_error $?' ERR
trap cleanup EXIT

if [[ $self_test_mode -eq 2 ]]; then
  false # Deliberate fault: the parent mode requires ERR to convert this to nonzero.
  printf 'ERR trap fault injection unexpectedly continued\n' >&2
  exit 0
fi
if [[ $self_test_mode -eq 1 ]]; then
  if self_test_output=$(bash "$0" --self-test-err-trap-child 2>&1); then
    self_test_status=0
  else
    self_test_status=$?
  fi
  if [[ $self_test_status -eq 0 ]]; then
    printf 'ERR trap self-test child unexpectedly exited zero\n' >&2
    exit 1
  fi
  if ! grep -Fq 'Phase 3 control-plane acceptance failed' <<<"$self_test_output"; then
    printf 'ERR trap self-test did not execute the acceptance error reporter\n' >&2
    printf '%s\n' "$self_test_output" >&2
    exit 1
  fi
  printf 'Phase 3 control-plane ERR trap self-test passed (child status %s)\n' \
    "$self_test_status"
  exit 0
fi

for command in curl docker jq nice ionice python3; do
  if ! command -v "$command" >/dev/null 2>&1; then
    printf 'required command is unavailable: %s\n' "$command" >&2
    exit 77
  fi
done

new_uuid() {
  local value
  value=$(tr 'A-F' 'a-f' </proc/sys/kernel/random/uuid)
  if [[ ! $value =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[47][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]]; then
    printf 'kernel did not provide a UUIDv4/v7 identifier\n' >&2
    return 1
  fi
  printf '%s\n' "$value"
}

build_recorder() {
  local manifest="$repo_root/fixtures/x11/Cargo.toml"
  local cargo_binary invoking_home invoking_cargo
  local cargo_args=(
    build --quiet --release --locked --jobs 2
    --manifest-path "$manifest"
    --bin x11-event-recorder
    --target "$fixture_rust_target"
  )

  if [[ -n ${SUDO_UID:-} && $SUDO_UID != 0 ]]; then
    invoking_home=$(getent passwd "$SUDO_UID" | cut -d: -f6)
    invoking_cargo="$invoking_home/.cargo/bin/cargo"
    if [[ ! -x $invoking_cargo ]]; then
      printf 'cargo is unavailable for invoking UID %s\n' "$SUDO_UID" >&2
      exit 77
    fi
    sudo -H -u "#$SUDO_UID" nice -n 15 ionice -c 3 \
      "$invoking_cargo" "${cargo_args[@]}"
  else
    cargo_binary=$(command -v cargo || true)
    if [[ -z $cargo_binary ]]; then
      printf 'cargo is required to build the independent X11 recorder\n' >&2
      exit 77
    fi
    nice -n 15 ionice -c 3 "$cargo_binary" "${cargo_args[@]}"
  fi

  if [[ ! -x $repo_root/fixtures/x11/target/$fixture_rust_target/release/x11-event-recorder ]]; then
    printf 'X11 recorder build did not produce its expected binary\n' >&2
    return 1
  fi
}

run_sdk_smoke() {
  local cargo_binary invoking_home invoking_cargo sdk_binary sdk_output
  local cargo_args=(
    build --quiet --locked --jobs 2
    --manifest-path "$repo_root/Cargo.toml"
    --target-dir "$repo_root/target"
    --target "$fixture_rust_target"
    --package xenoteer-sdk
    --example phase3-control-smoke
  )

  if [[ -n ${SUDO_UID:-} && $SUDO_UID != 0 ]]; then
    invoking_home=$(getent passwd "$SUDO_UID" | cut -d: -f6)
    invoking_cargo="$invoking_home/.cargo/bin/cargo"
    if [[ ! -x $invoking_cargo ]]; then
      printf 'cargo is unavailable for invoking UID %s\n' "$SUDO_UID" >&2
      exit 77
    fi
    sudo -H -u "#$SUDO_UID" nice -n 15 ionice -c 3 \
      "$invoking_cargo" "${cargo_args[@]}"
  else
    cargo_binary=$(command -v cargo || true)
    if [[ -z $cargo_binary ]]; then
      printf 'cargo is required to build the Phase 3 SDK smoke example\n' >&2
      exit 77
    fi
    nice -n 15 ionice -c 3 "$cargo_binary" "${cargo_args[@]}"
  fi

  sdk_binary="$repo_root/target/$fixture_rust_target/debug/examples/phase3-control-smoke"
  sdk_output="$test_dir/sdk-smoke.json"
  if [[ ! -x $sdk_binary ]]; then
    printf 'SDK smoke build did not produce its expected binary\n' >&2
    return 1
  fi
  nice -n 15 ionice -c 3 "$sdk_binary" \
    "$api_base" "$token_file" "$desktop_id" "$desktop_generation" >"$sdk_output"
  jq -e '
    .lifecycle == "succeeded"
      and .effect_stage == "none"
      and .outcome == {"type":"probe","ready":true}
      and (.command_id | type == "string")
  ' "$sdk_output" >/dev/null
}

assert_fixture_platform() {
  local image_os image_arch image_variant rust_host rust_arch expected_arch
  local rustc_binary invoking_home
  image_os=$(docker image inspect "$image" --format '{{.Os}}')
  image_arch=$(docker image inspect "$image" --format '{{.Architecture}}')
  image_variant=$(docker image inspect "$image" --format '{{.Variant}}')
  if [[ $image_variant == '<no value>' ]]; then
    image_variant=
  fi
  if [[ -n ${SUDO_UID:-} && $SUDO_UID != 0 ]]; then
    invoking_home=$(getent passwd "$SUDO_UID" | cut -d: -f6)
    rustc_binary="$invoking_home/.cargo/bin/rustc"
  else
    rustc_binary=$(command -v rustc || true)
  fi
  if [[ ! -x $rustc_binary ]]; then
    printf 'rustc is required to verify the recorder target architecture\n' >&2
    exit 77
  fi
  if [[ -n ${SUDO_UID:-} && $SUDO_UID != 0 ]]; then
    rust_host=$(sudo -H -u "#$SUDO_UID" "$rustc_binary" -vV \
      | awk '$1 == "host:" { print $2 }')
  else
    rust_host=$("$rustc_binary" -vV | awk '$1 == "host:" { print $2 }')
  fi
  if [[ -z $rust_host ]]; then
    printf 'rustc did not report a host target triple\n' >&2
    exit 77
  fi
  fixture_rust_target=$rust_host
  rust_arch=${rust_host%%-*}
  case "$rust_arch" in
    x86_64) expected_arch=amd64 ;;
    aarch64) expected_arch=arm64 ;;
    armv7*) expected_arch=arm ;;
    i?86) expected_arch=386 ;;
    *)
      printf 'unsupported recorder Rust host architecture: %s\n' "$rust_host" >&2
      exit 77
      ;;
  esac
  if [[ $image_os != linux || $rust_host != *-linux-* ]]; then
    printf 'recorder host/image OS mismatch: Rust %s, image %s/%s\n' \
      "$rust_host" "$image_os" "$image_arch" >&2
    exit 77
  fi
  if [[ $image_arch != "$expected_arch" ]]; then
    printf 'recorder host/image architecture mismatch: Rust %s, image %s/%s%s\n' \
      "$rust_host" "$image_os" "$image_arch" "${image_variant:+/$image_variant}" >&2
    exit 77
  fi
  if [[ $image_arch == arm && -n $image_variant && $rust_arch == armv7* \
    && $image_variant != v7 ]]; then
    printf 'recorder host/image ARM variant mismatch: Rust %s, image %s\n' \
      "$rust_host" "$image_variant" >&2
    exit 77
  fi
}

assert_fixture_linkage() {
  local linkage_status linkage_output
  if linkage_output=$(docker exec "$container_name" ldd \
    /run/xenoteer/phase3-control-plane/x11-event-recorder 2>&1); then
    linkage_status=0
  else
    linkage_status=$?
  fi
  if grep -Eqi 'not found|version [`'"'"'][^`'"'"']+[`'"'"'] not found' \
    <<<"$linkage_output"; then
    printf 'recorder is ABI-incompatible with the tested image:\n%s\n' \
      "$linkage_output" >&2
    return 1
  fi
  if [[ $linkage_status -ne 0 ]] \
    && ! grep -Eqi 'not a dynamic executable|statically linked' <<<"$linkage_output"; then
    printf 'could not validate recorder linkage inside the tested image:\n%s\n' \
      "$linkage_output" >&2
    return 1
  fi
}

expect_status() {
  local actual=$1 expected=$2 body_file=$3 description=$4
  if [[ $actual == "$expected" ]]; then
    return 0
  fi
  printf '%s returned HTTP %s, expected %s\n' \
    "$description" "$actual" "$expected" >&2
  if [[ -s $body_file ]]; then
    jq -c . "$body_file" >&2 2>/dev/null || sed -n '1,40p' "$body_file" >&2
  fi
  return 1
}

assert_json_content_type() {
  local headers=$1 expected=$2
  awk -v expected="$expected" '
    BEGIN { found = 0 }
    tolower($1) == "content-type:" {
      value = tolower($2)
      sub(/\r$/, "", value)
      split(value, parts, ";")
      if (parts[1] == expected) found = 1
    }
    END { exit found ? 0 : 1 }
  ' "$headers"
}

authenticated_request() {
  local method=$1 path=$2 body_file=$3 response_body=$4 response_headers=$5
  local idempotency_key=${6:-}
  local curl_args=(
    --config "$curl_auth_config"
    --silent --show-error
    --connect-timeout 3 --max-time 12
    --max-filesize 1048576
    --request "$method"
    --url "$api_base$path"
    --output "$response_body"
    --dump-header "$response_headers"
    --write-out '%{http_code}'
    --header 'Accept: application/json, application/problem+json'
  )
  if [[ -n $body_file ]]; then
    curl_args+=(--header 'Content-Type: application/json' --data-binary "@$body_file")
  fi
  if [[ -n $idempotency_key ]]; then
    curl_args+=(--header "Idempotency-Key: $idempotency_key")
  fi
  curl "${curl_args[@]}"
}

wait_terminal() {
  local command_id=$1 output=$2
  local headers="$test_dir/wait-$command_id.headers"
  local attempt status lifecycle
  for attempt in {1..12}; do
    status=$(authenticated_request GET \
      "/v1/desktops/$desktop_id/commands/$command_id/wait?timeout_ms=5000" \
      '' "$output" "$headers")
    case "$status" in
      200)
        lifecycle=$(jq -er '.lifecycle' "$output")
        case "$lifecycle" in
          succeeded|failed|cancelled_before_effect|cancelled_after_effect|deadline_before_effect|deadline_after_effect)
            assert_json_content_type "$headers" application/json
            return 0
            ;;
          *)
            printf 'terminal wait returned nonterminal lifecycle: %s\n' "$lifecycle" >&2
            return 1
            ;;
        esac
        ;;
      202) ;;
      *)
        expect_status "$status" 200 "$output" "command wait attempt $attempt"
        return 1
        ;;
    esac
  done
  printf 'command %s did not become terminal within the bounded wait window\n' \
    "$command_id" >&2
  return 1
}

assert_authentication_failure() {
  local description=$1
  shift
  local body="$test_dir/auth-negative-body.json"
  local headers="$test_dir/auth-negative-headers"
  local status
  status=$(curl --silent --show-error --connect-timeout 3 --max-time 8 \
    --max-filesize 1048576 \
    --output "$body" --dump-header "$headers" --write-out '%{http_code}' \
    "$@" "$api_base/v1/status")
  expect_status "$status" 401 "$body" "$description"
  assert_json_content_type "$headers" application/problem+json
  jq -e '.status == 401 and .code == "authentication_required"' "$body" >/dev/null
  awk '
    tolower($1) == "www-authenticate:" && $2 == "Bearer" && $3 == "realm=\"xenoteer\"\r" {
      found = 1
    }
    END { exit found ? 0 : 1 }
  ' "$headers"
}

submit_and_wait() {
  local body_file=$1 command_id=$2 terminal_file=$3 description=$4
  local response="$test_dir/$command_id-submit.json"
  local headers="$test_dir/$command_id-submit.headers"
  local status
  status=$(authenticated_request POST "/v1/desktops/$desktop_id/commands" \
    "$body_file" "$response" "$headers" "$command_id")
  case "$status" in
    200)
      jq -e --arg command_id "$command_id" \
        '.command_id == $command_id and .lifecycle == "succeeded"' "$response" >/dev/null
      cp "$response" "$terminal_file"
      ;;
    202)
      jq -e --arg command_id "$command_id" \
        '.command_id == $command_id and (.lifecycle == "accepted" or .lifecycle == "running")' \
        "$response" >/dev/null
      wait_terminal "$command_id" "$terminal_file"
      ;;
    *)
      expect_status "$status" 202 "$response" "$description submission"
      return 1
      ;;
  esac
  jq -e --arg command_id "$command_id" \
    '.command_id == $command_id and .lifecycle == "succeeded"' "$terminal_file" >/dev/null
}

assert_submission_snapshot() {
  local status=$1 response=$2 command_id=$3 description=$4
  case "$status" in
    200|202)
      jq -e --arg command_id "$command_id" '
        .command_id == $command_id
          and (.lifecycle == "accepted" or .lifecycle == "running" or .lifecycle == "succeeded")
      ' "$response" >/dev/null
      ;;
    *)
      expect_status "$status" 202 "$response" "$description"
      return 1
      ;;
  esac
}

submit_concurrent_duplicate() {
  local body_file=$1 command_id=$2 terminal_file=$3 description=$4
  local first_body="$test_dir/$command_id-concurrent-first.json"
  local first_headers="$test_dir/$command_id-concurrent-first.headers"
  local first_status_file="$test_dir/$command_id-concurrent-first.status"
  local second_body="$test_dir/$command_id-concurrent-second.json"
  local second_headers="$test_dir/$command_id-concurrent-second.headers"
  local second_status_file="$test_dir/$command_id-concurrent-second.status"
  local first_pid second_pid first_status second_status

  authenticated_request POST "/v1/desktops/$desktop_id/commands" \
    "$body_file" "$first_body" "$first_headers" "$command_id" >"$first_status_file" &
  first_pid=$!
  authenticated_request POST "/v1/desktops/$desktop_id/commands" \
    "$body_file" "$second_body" "$second_headers" "$command_id" >"$second_status_file" &
  second_pid=$!
  wait "$first_pid"
  wait "$second_pid"
  first_status=$(<"$first_status_file")
  second_status=$(<"$second_status_file")
  assert_submission_snapshot "$first_status" "$first_body" "$command_id" \
    "$description first concurrent submission"
  assert_submission_snapshot "$second_status" "$second_body" "$command_id" \
    "$description second concurrent submission"
  wait_terminal "$command_id" "$terminal_file"
  jq -e --arg command_id "$command_id" '
    .command_id == $command_id and .lifecycle == "succeeded"
  ' "$terminal_file" >/dev/null
}

abort_json_response() {
  local method=$1 path=$2 body_file=$3 idempotency_key=$4 description=$5
  local expected_status_pattern=$6
  local response_body="$test_dir/aborted-response-body"
  local response_headers="$test_dir/aborted-response-headers"
  local curl_status response_bytes response_status
  local curl_args=(
    --config "$curl_auth_config"
    --silent --show-error
    --connect-timeout 3 --max-time 15
    --max-filesize 1
    --request "$method"
    --url "$api_base$path"
    --output "$response_body"
    --dump-header "$response_headers"
    --header 'Accept: application/json, application/problem+json'
  )
  if [[ -n $body_file ]]; then
    curl_args+=(--header 'Content-Type: application/json' --data-binary "@$body_file")
  fi
  if [[ -n $idempotency_key ]]; then
    curl_args+=(--header "Idempotency-Key: $idempotency_key")
  fi

  rm -f "$response_body" "$response_headers"
  if curl "${curl_args[@]}" >/dev/null 2>"$test_dir/aborted-response-curl.err"; then
    curl_status=0
  else
    curl_status=$?
  fi
  if [[ $curl_status -ne 63 ]]; then
    printf '%s did not produce the intentional curl response abort (status %s)\n' \
      "$description" "$curl_status" >&2
    sed -n '1,20p' "$test_dir/aborted-response-curl.err" >&2
    return 1
  fi
  response_status=$(awk '/^HTTP\// { status = $2 } END { print status }' \
    "$response_headers")
  if [[ ! $response_status =~ ^($expected_status_pattern)$ ]]; then
    printf '%s reached unexpected HTTP status %s before disconnect\n' \
      "$description" "${response_status:-missing}" >&2
    return 1
  fi
  assert_json_content_type "$response_headers" application/json
  if [[ -e $response_body ]]; then
    response_bytes=$(wc -c <"$response_body")
  else
    response_bytes=0
  fi
  if ((response_bytes > 1)) || jq -e . "$response_body" >/dev/null 2>&1; then
    printf '%s unexpectedly retained a complete response after the forced disconnect\n' \
      "$description" >&2
    return 1
  fi
}

run_restricted_grant_test() {
  local port_binding host_port restricted_base
  local api_base ready status_body status_headers status
  local restricted_desktop_id
  local observe_id observe_body observe_headers observe_status
  local input_body input_headers input_status restricted_exit

  docker run --detach \
    --name "$restricted_container_name" \
    --cpus 2 \
    --memory 4g \
    --pids-limit 512 \
    --shm-size 4g \
    --log-driver json-file \
    --log-opt max-size=2m \
    --log-opt max-file=1 \
    --publish '127.0.0.1::8080' \
    --env 'XENOTEER__AUTH__GRANTS=["desktop:status"]' \
    --volume "$token_file:/run/secrets/xenoteer_api_token:ro" \
    "$image" >/dev/null
  restricted_container_created=1

  port_binding=$(docker port "$restricted_container_name" 8080/tcp)
  if [[ ! $port_binding =~ ^127\.0\.0\.1:([0-9]{1,5})$ ]]; then
    printf 'Docker returned an unexpected restricted API binding: %s\n' \
      "$port_binding" >&2
    return 1
  fi
  host_port=${BASH_REMATCH[1]}
  restricted_base="http://127.0.0.1:$host_port"
  api_base=$restricted_base

  ready=0
  for _ in {1..90}; do
    if [[ $(curl --silent --output /dev/null --connect-timeout 1 --max-time 2 \
      --max-filesize 1048576 --write-out '%{http_code}' \
      "$api_base/readyz" || true) == 200 ]]; then
      ready=1
      break
    fi
    if [[ $(docker inspect "$restricted_container_name" \
      --format '{{.State.Running}}') != true ]]; then
      printf 'restricted-grant container stopped before readiness\n' >&2
      return 1
    fi
    sleep 1
  done
  if [[ $ready -ne 1 ]]; then
    printf 'restricted-grant container did not become ready\n' >&2
    return 1
  fi

  status_body="$test_dir/restricted-status.json"
  status_headers="$test_dir/restricted-status.headers"
  status=$(authenticated_request GET /v1/status '' "$status_body" "$status_headers")
  expect_status "$status" 200 "$status_body" 'restricted desktop-status grant'
  jq -e '.desktop.state == "ready" and (.desktop.id | type == "string")' \
    "$status_body" >/dev/null
  restricted_desktop_id=$(jq -er '.desktop.id' "$status_body")

  observe_id=$(new_uuid)
  observe_body="$test_dir/restricted-observe.json"
  observe_headers="$test_dir/restricted-observe.headers"
  observe_status=$(authenticated_request GET \
    "/v1/desktops/$restricted_desktop_id/commands/$observe_id" '' \
    "$observe_body" "$observe_headers")
  expect_status "$observe_status" 403 "$observe_body" \
    'restricted principal command observation'
  assert_json_content_type "$observe_headers" application/problem+json
  jq -e '.status == 403 and .code == "permission_denied" and .retry == "never"' \
    "$observe_body" >/dev/null

  input_body="$test_dir/restricted-input.json"
  input_headers="$test_dir/restricted-input.headers"
  input_status=$(authenticated_request GET \
    "/v1/desktops/$restricted_desktop_id/lease" '' \
    "$input_body" "$input_headers")
  expect_status "$input_status" 403 "$input_body" \
    'restricted principal input-control access'
  assert_json_content_type "$input_headers" application/problem+json
  jq -e '.status == 403 and .code == "permission_denied" and .retry == "never"' \
    "$input_body" >/dev/null

  docker stop --time 40 "$restricted_container_name" >/dev/null
  restricted_exit=$(docker inspect "$restricted_container_name" \
    --format '{{.State.ExitCode}}')
  if [[ $restricted_exit -ne 0 ]]; then
    printf 'restricted-grant container returned exit code %s\n' \
      "$restricted_exit" >&2
    return 1
  fi
  docker logs "$restricted_container_name" \
    >"$test_dir/final-restricted-container.log" 2>&1
  if grep -Fq -- "$token_canary" "$test_dir/final-restricted-container.log"; then
    printf 'restricted-grant container logs exposed the API-token canary\n' >&2
    return 1
  fi
}

image=$(docker image inspect "$image_reference" --format '{{.Id}}')
if [[ ! $image =~ ^sha256:[0-9a-f]{64}$ ]]; then
  printf 'resolved image has an invalid immutable ID: %s\n' "$image" >&2
  exit 1
fi
assert_fixture_platform
build_recorder

printf '%s' "$token_canary" >"$token_file"
chmod 0400 "$token_file"
if [[ $(id -u) -eq 0 ]]; then
  chown 0:0 "$token_file"
elif [[ $(docker info --format '{{json .SecurityOptions}}') == *'name=rootless'* ]]; then
  : # The rootless daemon maps this owner to UID 0 inside the container.
else
  printf 'run as root or use rootless Docker so the token mount is container-root-owned\n' >&2
  exit 77
fi
printf 'header = "Authorization: Bearer %s"\n' "$token_canary" >"$curl_auth_config"
chmod 0600 "$curl_auth_config"

docker run --detach \
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
  "$image" >/dev/null
container_created=1

port_binding=$(docker port "$container_name" 8080/tcp)
if [[ ! $port_binding =~ ^127\.0\.0\.1:([0-9]{1,5})$ ]]; then
  printf 'Docker returned an unexpected control-plane binding: %s\n' "$port_binding" >&2
  exit 1
fi
host_port=${BASH_REMATCH[1]}
if ((host_port < 1 || host_port > 65535)); then
  printf 'Docker returned an invalid dynamic host port\n' >&2
  exit 1
fi
api_base="http://127.0.0.1:$host_port"

ready=0
for _ in {1..90}; do
  if [[ $(curl --silent --output /dev/null --connect-timeout 1 --max-time 2 \
    --max-filesize 1048576 \
    --write-out '%{http_code}' "$api_base/readyz" || true) == 200 ]]; then
    ready=1
    break
  fi
  if [[ $(docker inspect "$container_name" --format '{{.State.Running}}') != true ]]; then
    printf 'container stopped before the control plane became ready\n' >&2
    exit 1
  fi
  sleep 1
done
if [[ $ready -ne 1 ]]; then
  printf 'control plane did not become ready within 90 seconds\n' >&2
  exit 1
fi

assert_authentication_failure 'missing Bearer credential'
assert_authentication_failure 'invalid Bearer credential' \
  --header 'Authorization: Bearer invalid-token-material-with-32-bytes-0000'
assert_authentication_failure 'wrong authentication scheme' \
  --header 'Authorization: Basic bm90LWEtdG9rZW4='
assert_authentication_failure 'duplicate Authorization headers' \
  --header 'Authorization: Bearer duplicate-invalid-token-material-0000' \
  --header 'Authorization: Bearer duplicate-invalid-token-material-0000'

status_body="$test_dir/status.json"
status_headers="$test_dir/status.headers"
status=$(authenticated_request GET /v1/status '' "$status_body" "$status_headers")
expect_status "$status" 200 "$status_body" 'authenticated status'
assert_json_content_type "$status_headers" application/json
jq -e '
  .protocol_min == {"major":1,"minor":0}
    and .protocol_max == {"major":1,"minor":0}
    and .desktop.state == "ready"
    and (.desktop.id | type == "string")
    and (.desktop.generation | type == "string")
' "$status_body" >/dev/null
desktop_id=$(jq -er '.desktop.id' "$status_body")
desktop_generation=$(jq -er '.desktop.generation' "$status_body")
nice -n 15 ionice -c 3 "$repo_root/scripts/container/test-phase3-websocket.py" \
  exercise --api-base "$api_base" --token-file "$token_file" \
  --desktop-id "$desktop_id" --desktop-generation "$desktop_generation"
run_sdk_smoke

lease_request="$test_dir/lease-acquire.json"
jq -n \
  --arg request_id "$(new_uuid)" \
  --arg desktop_id "$desktop_id" \
  --arg generation "$desktop_generation" '
  {
    protocol_version: {major: 1, minor: 0},
    request_id: $request_id,
    desktop_id: $desktop_id,
    desktop_generation: $generation,
    ttl_ms: 60000
  }
' >"$lease_request"
lease_body="$test_dir/lease.json"
lease_headers="$test_dir/lease.headers"
lease_status=$(authenticated_request POST "/v1/desktops/$desktop_id/lease" \
  "$lease_request" "$lease_body" "$lease_headers")
expect_status "$lease_status" 201 "$lease_body" 'lease acquisition'
jq -e \
  --arg desktop_id "$desktop_id" --arg generation "$desktop_generation" '
  .desktop_id == $desktop_id
    and .desktop_generation == $generation
    and .state == "held_by_caller"
    and (.lease_id | type == "string")
    and (.expires_at | type == "string")
' "$lease_body" >/dev/null
lease_id=$(jq -er '.lease_id' "$lease_body")
lease_expiry=$(jq -er '.expires_at' "$lease_body")

renew_request="$test_dir/lease-renew.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg desktop_id "$desktop_id" \
  --arg generation "$desktop_generation" --arg lease_id "$lease_id" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    desktop_id: $desktop_id, desktop_generation: $generation,
    lease_id: $lease_id, ttl_ms: 60000
  }
' >"$renew_request"
renew_body="$test_dir/lease-renew-response.json"
renew_headers="$test_dir/lease-renew.headers"
renew_status=$(authenticated_request POST \
  "/v1/desktops/$desktop_id/lease/$lease_id/renew" "$renew_request" \
  "$renew_body" "$renew_headers")
expect_status "$renew_status" 200 "$renew_body" 'lease renewal'
jq -e --arg lease_id "$lease_id" --arg previous_expiry "$lease_expiry" '
  .state == "held_by_caller" and .lease_id == $lease_id
    and .expires_at > $previous_expiry
' "$renew_body" >/dev/null

occupied_request="$test_dir/lease-conflicting-acquire.json"
jq --arg request_id "$(new_uuid)" '.request_id = $request_id' \
  "$lease_request" >"$occupied_request"
occupied_body="$test_dir/lease-conflicting-acquire-response.json"
occupied_headers="$test_dir/lease-conflicting-acquire.headers"
occupied_status=$(authenticated_request POST "/v1/desktops/$desktop_id/lease" \
  "$occupied_request" "$occupied_body" "$occupied_headers")
expect_status "$occupied_status" 409 "$occupied_body" 'occupied lease acquisition'
assert_json_content_type "$occupied_headers" application/problem+json
jq -e '.status == 409 and .code == "lease_conflict" and .retry == "after_backoff"' \
  "$occupied_body" >/dev/null

launch_command_id=$(new_uuid)
launch_request="$test_dir/launch.json"
jq -n \
  --arg request_id "$(new_uuid)" \
  --arg command_id "$launch_command_id" \
  --arg desktop_id "$desktop_id" \
  --arg generation "$desktop_generation" '
  {
    protocol_version: {major: 1, minor: 0},
    request_id: $request_id,
    command_id: $command_id,
    desktop_id: $desktop_id,
    desktop_generation: $generation,
    lease_id: null,
    deadline: null,
    trace_policy: "detailed",
    command: {
      type: "application_launch",
      application: "xmessage",
      arguments: ["Xenoteer Phase 3 acceptance"]
    }
  }
' >"$launch_request"
launch_terminal="$test_dir/launch-terminal.json"
submit_concurrent_duplicate "$launch_request" "$launch_command_id" "$launch_terminal" \
  'deduplicated xmessage launch'
jq -e '
  .effect_stage == "process_started"
    and .outcome.type == "application_launched"
    and (.outcome.process.pid | type == "number" and . > 0)
    and (
      .outcome.process.proc_start_ticks
      | type == "string" and test("^[1-9][0-9]{0,19}$")
    )
    and (.outcome.process.launch_id | type == "string")
' "$launch_terminal" >/dev/null
process_reference=$(jq -c '.outcome.process' "$launch_terminal")
process_reference_file="$test_dir/process-reference.json"
printf '%s\n' "$process_reference" >"$process_reference_file"
process_pid=$(jq -er '.pid' <<<"$process_reference")
process_start_ticks=$(jq -er '.proc_start_ticks' <<<"$process_reference")
docker exec "$container_name" sh -eu -c '
  pid=$1
  expected_ticks=$2
  test -r "/proc/$pid/status"
  test "$(awk '\''$1 == "Uid:" { print $2 }'\'' "/proc/$pid/status")" -eq 1000
  test "$(awk '\''{ print $22 }'\'' "/proc/$pid/stat")" -eq "$expected_ticks"
  test "$(/command/s6-setuidgid xenoteer readlink "/proc/$pid/exe")" = /usr/bin/xmessage
  actual=$(/command/s6-setuidgid xenoteer cat "/proc/$pid/cmdline" | tr "\000" "\n")
  expected=$(printf "%s\n" /usr/bin/xmessage -center "Xenoteer Phase 3 acceptance")
  test "$actual" = "$expected"
  matching=$(pgrep -u 1000 -x xmessage)
  test "$(printf "%s\n" "$matching" | wc -l)" -eq 1
  test "$matching" -eq "$pid"
' sh "$process_pid" "$process_start_ticks"

changed_launch_request="$test_dir/launch-changed.json"
jq --arg request_id "$(new_uuid)" '
  .request_id = $request_id | .command.arguments = ["changed duplicate body"]
' "$launch_request" >"$changed_launch_request"
changed_launch_body="$test_dir/launch-changed-response.json"
changed_launch_headers="$test_dir/launch-changed.headers"
changed_launch_status=$(authenticated_request POST "/v1/desktops/$desktop_id/commands" \
  "$changed_launch_request" "$changed_launch_body" "$changed_launch_headers" \
  "$launch_command_id")
expect_status "$changed_launch_status" 409 "$changed_launch_body" \
  'changed-body concurrent command retry'
assert_json_content_type "$changed_launch_headers" application/problem+json
jq -e '.status == 409 and .code == "command_id_conflict" and .retry == "never"' \
  "$changed_launch_body" >/dev/null

xmessage_mapped=0
for _ in {1..100}; do
  xmessage_windows=$(docker exec "$container_name" /command/s6-envdir /run/xenoteer/env \
    /command/s6-setuidgid xenoteer sh -eu -c '
      expected_pid=$1
      expected_machine=$(hostname)
      expected_command="\"/usr/bin/xmessage\", \"-center\", \"Xenoteer Phase 3 acceptance\""
      xprop -root _NET_CLIENT_LIST 2>/dev/null \
        | grep -Eo "0x[0-9a-fA-F]+" \
        | while IFS= read -r window; do
            class=$(xprop -id "$window" WM_CLASS 2>/dev/null || true)
            case "$class" in
              *xmessage*|*Xmessage*) ;;
              *) continue ;;
            esac

            command=$(xprop -id "$window" WM_COMMAND 2>/dev/null || true)
            printf "%s\n" "$command" | grep -Fq -- "$expected_command" || continue

            machine=$(xprop -id "$window" WM_CLIENT_MACHINE 2>/dev/null || true)
            printf "%s\n" "$machine" | grep -Fq -- "\"$expected_machine\"" || continue

            # _NET_WM_PID is advisory and optional. xmessage does not publish it,
            # but reject a candidate if another client supplies a conflicting PID.
            pid=$(xprop -id "$window" _NET_WM_PID 2>/dev/null \
              | awk -F= "/_NET_WM_PID/ { gsub(/[[:space:]]/, \"\", \$2); print \$2 }")
            if test -n "$pid" && test "$pid" != "$expected_pid"; then
              continue
            fi
            printf "%s\n" "$window"
          done
    ' sh "$process_pid" 2>/dev/null | sed -n '1,2p' || true)
  xmessage_window_count=$(awk '
    /^0x[0-9a-fA-F]+$/ { count += 1 }
    END { print count + 0 }
  ' <<<"$xmessage_windows")
  xmessage_window=$(sed -n '1p' <<<"$xmessage_windows")
  if [[ $xmessage_window_count -eq 1 && $xmessage_window =~ ^0x[0-9a-fA-F]+$ ]] \
    && docker exec "$container_name" /command/s6-envdir /run/xenoteer/env \
      /command/s6-setuidgid xenoteer env LC_ALL=C xwininfo -id "$xmessage_window" \
      2>/dev/null | grep -Fq 'Map State: IsViewable'; then
    xmessage_mapped=1
    break
  fi
  sleep 0.1
done
if [[ $xmessage_mapped -ne 1 ]]; then
  printf 'registered xmessage process did not map one uniquely correlated X11 window\n' >&2
  docker exec "$container_name" /command/s6-envdir /run/xenoteer/env \
    /command/s6-setuidgid xenoteer sh -u -c '
      printf "test process PID: %s; container hostname: %s\n" "$1" "$(hostname)"
      xprop -root _NET_CLIENT_LIST 2>/dev/null \
        | grep -Eo "0x[0-9a-fA-F]+" \
        | while IFS= read -r window; do
            class=$(xprop -id "$window" WM_CLASS 2>/dev/null || true)
            case "$class" in
              *xmessage*|*Xmessage*)
                printf "window %s\n%s\n" "$window" "$class"
                xprop -id "$window" WM_COMMAND WM_CLIENT_MACHINE _NET_WM_PID \
                  2>/dev/null || true
                ;;
            esac
          done
    ' sh "$process_pid" >&2 || true
  exit 1
fi

fixture_dir=/run/xenoteer/phase3-control-plane
recorder_events=/run/user/1000/phase3-control-plane-events.jsonl
recorder_errors=/run/user/1000/phase3-control-plane-events.err
docker exec "$container_name" install -d -o 0 -g 0 -m 0755 "$fixture_dir"
docker cp "$repo_root/fixtures/x11/target/$fixture_rust_target/release/x11-event-recorder" \
  "$container_name:$fixture_dir/x11-event-recorder" >/dev/null
docker exec "$container_name" chown 0:0 "$fixture_dir/x11-event-recorder"
docker exec "$container_name" chmod 0555 "$fixture_dir/x11-event-recorder"
assert_fixture_linkage
docker exec "$container_name" rm -f "$recorder_events" "$recorder_errors"
docker exec --detach "$container_name" /command/s6-envdir /run/xenoteer/env \
  /command/s6-setuidgid xenoteer sh -c \
  "exec $fixture_dir/x11-event-recorder --focus-before-ready --max-events 512 >$recorder_events 2>$recorder_errors"

recorder_ready=0
for _ in {1..100}; do
  if docker exec "$container_name" grep -Fq '"type":"ready"' "$recorder_events" \
    2>/dev/null; then
    recorder_ready=1
    break
  fi
  sleep 0.1
done
if [[ $recorder_ready -ne 1 ]]; then
  docker exec "$container_name" sed -n '1,80p' "$recorder_errors" >&2 || true
  printf 'independent X11 recorder did not become ready\n' >&2
  exit 1
fi
recorder_window=$(docker exec "$container_name" sh -eu -c 'cat "$1"' sh "$recorder_events" \
  | jq -er 'select(.type == "ready") | .window' | sed -n '1p')
if [[ ! $recorder_window =~ ^[0-9]+$ ]]; then
  printf 'recorder returned an invalid X11 window ID\n' >&2
  exit 1
fi
docker exec "$container_name" sh -eu -c 'cat "$1"' sh "$recorder_events" \
  | jq -se --argjson window "$recorder_window" '
    any(.[];
      .type == "ready_metadata"
        and .window == $window
        and .painted == true
        and .focus_requested == true
        and .observed_focus == $window
        and .max_events == 512)
  ' >/dev/null
docker exec "$container_name" sh -eu -c '
  pid=$(pgrep -u 1000 -f "^/run/xenoteer/phase3-control-plane/x11-event-recorder --focus-before-ready --max-events 512$")
  test "$(printf "%s\n" "$pid" | wc -l)" -eq 1
  test "$(awk '\''$1 == "Uid:" { print $2 }'\'' "/proc/$pid/status")" -eq 1000
'

geometry=$(docker exec "$container_name" /command/s6-envdir /run/xenoteer/env \
  /command/s6-setuidgid xenoteer env LC_ALL=C xwininfo -id "$recorder_window")
window_x=$(awk -F: '/Absolute upper-left X:/ { gsub(/[[:space:]]/, "", $2); print $2 }' \
  <<<"$geometry")
window_y=$(awk -F: '/Absolute upper-left Y:/ { gsub(/[[:space:]]/, "", $2); print $2 }' \
  <<<"$geometry")
window_width=$(awk -F: '/^[[:space:]]*Width:/ { gsub(/[[:space:]]/, "", $2); print $2 }' \
  <<<"$geometry")
window_height=$(awk -F: '/^[[:space:]]*Height:/ { gsub(/[[:space:]]/, "", $2); print $2 }' \
  <<<"$geometry")
for value in "$window_x" "$window_y" "$window_width" "$window_height"; do
  if [[ ! $value =~ ^-?[0-9]+$ ]]; then
    printf 'could not parse recorder window geometry\n' >&2
    exit 1
  fi
done
if ((window_width < 400 || window_height < 200)); then
  printf 'recorder window is unexpectedly small: %sx%s\n' \
    "$window_width" "$window_height" >&2
  exit 1
fi
start_x=$((window_x + 80))
target_x=$((window_x + window_width - 80))
target_y=$((window_y + window_height / 2))

instant_command_id=$(new_uuid)
instant_request="$test_dir/pointer-instant.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg command_id "$instant_command_id" \
  --arg desktop_id "$desktop_id" --arg generation "$desktop_generation" \
  --arg lease_id "$lease_id" --argjson x "$start_x" --argjson y "$target_y" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    command_id: $command_id, desktop_id: $desktop_id,
    desktop_generation: $generation, lease_id: $lease_id,
    deadline: null, trace_policy: "detailed",
    command: {type: "pointer_move", target: {x: $x, y: $y}, duration_ms: 0, curve: "instant"}
  }
' >"$instant_request"
instant_terminal="$test_dir/pointer-instant-terminal.json"
submit_and_wait "$instant_request" "$instant_command_id" "$instant_terminal" \
  'pointer starting-position move'
jq -e '.effect_stage == "pointer_moved" and .outcome.type == "acknowledged"' \
  "$instant_terminal" >/dev/null

start_observed=0
for _ in {1..100}; do
  if docker exec "$container_name" sh -eu -c 'cat "$1"' sh "$recorder_events" \
    | jq -se --argjson x "$start_x" --argjson y "$target_y" \
      'any(.[]; .type == "motion" and .root_x == $x and .root_y == $y)' >/dev/null; then
    start_observed=1
    break
  fi
  sleep 0.1
done
if [[ $start_observed -ne 1 ]]; then
  printf 'recorder did not observe the established pointer start coordinate\n' >&2
  exit 1
fi
baseline_motion_count=$(docker exec "$container_name" sh -eu -c 'cat "$1"' sh "$recorder_events" \
  | jq -s '[.[] | select(.type == "motion")] | length')

smooth_command_id=$(new_uuid)
smooth_request="$test_dir/pointer-smooth.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg command_id "$smooth_command_id" \
  --arg desktop_id "$desktop_id" --arg generation "$desktop_generation" \
  --arg lease_id "$lease_id" --argjson x "$target_x" --argjson y "$target_y" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    command_id: $command_id, desktop_id: $desktop_id,
    desktop_generation: $generation, lease_id: $lease_id,
    deadline: null, trace_policy: "detailed",
    command: {type: "pointer_move", target: {x: $x, y: $y}, duration_ms: 1000, curve: "smooth"}
  }
' >"$smooth_request"
smooth_terminal="$test_dir/pointer-smooth-terminal.json"
submit_and_wait "$smooth_request" "$smooth_command_id" "$smooth_terminal" \
  'smooth pointer move'
jq -e '.effect_stage == "pointer_moved" and .outcome.type == "acknowledged"' \
  "$smooth_terminal" >/dev/null

endpoint_observed=0
for _ in {1..100}; do
  if docker exec "$container_name" sh -eu -c 'cat "$1"' sh "$recorder_events" \
    | jq -se --argjson x "$target_x" --argjson y "$target_y" \
      'any(.[]; .type == "motion" and .root_x == $x and .root_y == $y)' >/dev/null; then
    endpoint_observed=1
    break
  fi
  sleep 0.1
done
if [[ $endpoint_observed -ne 1 ]]; then
  printf 'recorder did not observe the smooth pointer endpoint\n' >&2
  exit 1
fi
docker cp "$container_name:$recorder_events" "$test_dir/recorder-events.jsonl" >/dev/null
jq -se \
  --argjson baseline "$baseline_motion_count" \
  --argjson start_x "$start_x" --argjson target_x "$target_x" \
  --argjson target_y "$target_y" '
  ([.[] | select(.type == "motion")][$baseline:]) as $motion
  | ($motion | length) >= 3
    and ($motion[-1].root_x == $target_x and $motion[-1].root_y == $target_y)
    and (
      [$motion[]
        | select(.root_y == $target_y and .root_x > $start_x and .root_x < $target_x)
        | [.root_x, .root_y]]
      | unique | length
    ) >= 2
' "$test_dir/recorder-events.jsonl" >/dev/null

retry_body="$test_dir/pointer-retry.json"
retry_headers="$test_dir/pointer-retry.headers"
retry_status=$(authenticated_request POST "/v1/desktops/$desktop_id/commands" \
  "$smooth_request" "$retry_body" "$retry_headers" "$smooth_command_id")
expect_status "$retry_status" 200 "$retry_body" 'exact terminal command retry'
jq -e --arg command_id "$smooth_command_id" '
  .command_id == $command_id and .lifecycle == "succeeded" and .effect_stage == "pointer_moved"
' "$retry_body" >/dev/null

changed_request="$test_dir/pointer-changed.json"
jq --arg request_id "$(new_uuid)" \
  '.request_id = $request_id | .command.duration_ms = 1100' \
  "$smooth_request" >"$changed_request"
conflict_body="$test_dir/pointer-conflict.json"
conflict_headers="$test_dir/pointer-conflict.headers"
conflict_status=$(authenticated_request POST "/v1/desktops/$desktop_id/commands" \
  "$changed_request" "$conflict_body" "$conflict_headers" "$smooth_command_id")
expect_status "$conflict_status" 409 "$conflict_body" 'changed-body command retry'
assert_json_content_type "$conflict_headers" application/problem+json
jq -e '.status == 409 and .code == "command_id_conflict" and .retry == "never"' \
  "$conflict_body" >/dev/null

# Fault-inject a lost POST response. curl sends the complete command request, then
# rejects the oversized response at one byte and closes the connection. Recovery
# below uses only ledger reads; it never replays the command submission.
lost_submit_command_id=$(new_uuid)
lost_submit_request="$test_dir/lost-submit.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg command_id "$lost_submit_command_id" \
  --arg desktop_id "$desktop_id" --arg generation "$desktop_generation" \
  --arg lease_id "$lease_id" --argjson x "$start_x" --argjson y "$target_y" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    command_id: $command_id, desktop_id: $desktop_id,
    desktop_generation: $generation, lease_id: $lease_id,
    deadline: null, trace_policy: "detailed",
    command: {type: "pointer_move", target: {x: $x, y: $y}, duration_ms: 600, curve: "smooth"}
  }
' >"$lost_submit_request"
abort_json_response POST "/v1/desktops/$desktop_id/commands" \
  "$lost_submit_request" "$lost_submit_command_id" 'lost command-submit response' \
  '200|202'
lost_submit_terminal="$test_dir/lost-submit-terminal.json"
wait_terminal "$lost_submit_command_id" "$lost_submit_terminal"
jq -e --arg command_id "$lost_submit_command_id" '
  .command_id == $command_id and .lifecycle == "succeeded"
    and .effect_stage == "pointer_moved" and .outcome.type == "acknowledged"
' "$lost_submit_terminal" >/dev/null

# Independently drop a long-poll result after a normally admitted command. The
# subsequent plain GET must return the retained terminal result for the same ID.
lost_wait_command_id=$(new_uuid)
lost_wait_request="$test_dir/lost-wait.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg command_id "$lost_wait_command_id" \
  --arg desktop_id "$desktop_id" --arg generation "$desktop_generation" \
  --arg lease_id "$lease_id" --argjson x "$target_x" --argjson y "$target_y" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    command_id: $command_id, desktop_id: $desktop_id,
    desktop_generation: $generation, lease_id: $lease_id,
    deadline: null, trace_policy: "detailed",
    command: {type: "pointer_move", target: {x: $x, y: $y}, duration_ms: 1200, curve: "smooth"}
  }
' >"$lost_wait_request"
lost_wait_submit_body="$test_dir/lost-wait-submit.json"
lost_wait_submit_headers="$test_dir/lost-wait-submit.headers"
lost_wait_submit_status=$(authenticated_request POST \
  "/v1/desktops/$desktop_id/commands" "$lost_wait_request" \
  "$lost_wait_submit_body" "$lost_wait_submit_headers" "$lost_wait_command_id")
assert_submission_snapshot "$lost_wait_submit_status" "$lost_wait_submit_body" \
  "$lost_wait_command_id" 'pre-disconnect long-poll command submission'
abort_json_response GET \
  "/v1/desktops/$desktop_id/commands/$lost_wait_command_id/wait?timeout_ms=5000" \
  '' '' 'lost command-wait response' '200'
lost_wait_terminal="$test_dir/lost-wait-terminal.json"
lost_wait_headers="$test_dir/lost-wait-terminal.headers"
lost_wait_status=$(authenticated_request GET \
  "/v1/desktops/$desktop_id/commands/$lost_wait_command_id" '' \
  "$lost_wait_terminal" "$lost_wait_headers")
expect_status "$lost_wait_status" 200 "$lost_wait_terminal" \
  'ledger read after disconnected long poll'
jq -e --arg command_id "$lost_wait_command_id" '
  .command_id == $command_id and .lifecycle == "succeeded"
    and .effect_stage == "pointer_moved" and .outcome.type == "acknowledged"
' "$lost_wait_terminal" >/dev/null

terminate_terminal="$test_dir/terminate-terminal.json"
nice -n 15 ionice -c 3 "$repo_root/scripts/container/test-phase3-websocket.py" \
  process-terminate --api-base "$api_base" --token-file "$token_file" \
  --desktop-id "$desktop_id" --desktop-generation "$desktop_generation" \
  --process-file "$process_reference_file" --result-file "$terminate_terminal"
jq -e --argjson process "$process_reference" '
  .effect_stage == "process_exited"
    and .outcome.type == "process_terminated"
    and .outcome.process.process == $process
    and .outcome.process.state == "exited"
    and .outcome.process.exit == {code:null, signal:15, core_dumped:false}
' "$terminate_terminal" >/dev/null

status_command_id=$(new_uuid)
process_status_request="$test_dir/process-status.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg command_id "$status_command_id" \
  --arg desktop_id "$desktop_id" --arg generation "$desktop_generation" \
  --argjson process "$process_reference" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    command_id: $command_id, desktop_id: $desktop_id,
    desktop_generation: $generation, lease_id: null,
    deadline: null, trace_policy: "normal",
    command: {type: "process_status", process: $process}
  }
' >"$process_status_request"
process_status_terminal="$test_dir/process-status-terminal.json"
submit_and_wait "$process_status_request" "$status_command_id" "$process_status_terminal" \
  'post-termination process status'
jq -e --argjson process "$process_reference" '
  .outcome.type == "process_status"
    and .outcome.process.process == $process
    and .outcome.process.state == "exited"
    and (.outcome.process.exit != null)
' "$process_status_terminal" >/dev/null
docker exec "$container_name" sh -eu -c '
  pid=$1
  expected_ticks=$2
  if test -r "/proc/$pid/stat"; then
    current_ticks=$(awk '\''{ print $22 }'\'' "/proc/$pid/stat")
    test "$current_ticks" -ne "$expected_ticks"
  fi
' sh "$process_pid" "$process_start_ticks"
zombie_free=0
for _ in {1..20}; do
  zombie_states=$(docker exec "$container_name" ps -eo stat=)
  if ! grep -Eq '^[[:space:]]*Z' <<<"$zombie_states"; then
    zombie_free=1
    break
  fi
  sleep 0.1
done
if [[ $zombie_free -ne 1 ]]; then
  printf 'a zombie remained after the registered xmessage process was reaped\n' >&2
  docker exec "$container_name" ps -eo pid,ppid,pgid,stat,comm >&2 || true
  exit 1
fi

release_request="$test_dir/lease-release.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg desktop_id "$desktop_id" \
  --arg generation "$desktop_generation" --arg lease_id "$lease_id" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    desktop_id: $desktop_id, desktop_generation: $generation, lease_id: $lease_id
  }
' >"$release_request"
release_body="$test_dir/lease-release-response.json"
release_headers="$test_dir/lease-release.headers"
release_status=$(authenticated_request DELETE \
  "/v1/desktops/$desktop_id/lease/$lease_id" "$release_request" \
  "$release_body" "$release_headers")
expect_status "$release_status" 200 "$release_body" 'lease release and input reset'
jq -e --arg desktop_id "$desktop_id" --arg generation "$desktop_generation" '
  .desktop_id == $desktop_id and .desktop_generation == $generation
    and .state == "vacant" and .lease_id == null and .expires_at == null
' "$release_body" >/dev/null

lease_state_body="$test_dir/lease-state.json"
lease_state_headers="$test_dir/lease-state.headers"
lease_state_status=$(authenticated_request GET "/v1/desktops/$desktop_id/lease" '' \
  "$lease_state_body" "$lease_state_headers")
expect_status "$lease_state_status" 200 "$lease_state_body" 'post-reset lease state'
jq -e '.state == "vacant" and .lease_id == null and .expires_at == null' \
  "$lease_state_body" >/dev/null

# Acquire a deliberately short lease, leave a real physical button owned by
# Xenoteer, and require TTL expiry to complete the conservative reset before a
# new controller can enter.
short_lease_request="$test_dir/short-lease-acquire.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg desktop_id "$desktop_id" \
  --arg generation "$desktop_generation" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    desktop_id: $desktop_id, desktop_generation: $generation, ttl_ms: 10000
  }
' >"$short_lease_request"
short_lease_body="$test_dir/short-lease.json"
short_lease_headers="$test_dir/short-lease.headers"
short_lease_status=$(authenticated_request POST "/v1/desktops/$desktop_id/lease" \
  "$short_lease_request" "$short_lease_body" "$short_lease_headers")
expect_status "$short_lease_status" 201 "$short_lease_body" 'short lease acquisition'
short_lease_id=$(jq -er '.lease_id' "$short_lease_body")
if [[ $short_lease_id == "$lease_id" ]]; then
  printf 'lease reacquisition reused an opaque lease capability\n' >&2
  exit 1
fi

button_event_baseline=$(docker exec "$container_name" sh -eu -c 'cat "$1"' \
  sh "$recorder_events" | jq -s 'length')
button_down_command_id=$(new_uuid)
button_down_request="$test_dir/button-down.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg command_id "$button_down_command_id" \
  --arg desktop_id "$desktop_id" --arg generation "$desktop_generation" \
  --arg lease_id "$short_lease_id" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    command_id: $command_id, desktop_id: $desktop_id,
    desktop_generation: $generation, lease_id: $lease_id,
    deadline: null, trace_policy: "detailed",
    command: {type: "pointer_button_down", button: 1, allow_redundant: false}
  }
' >"$button_down_request"
button_down_terminal="$test_dir/button-down-terminal.json"
submit_and_wait "$button_down_request" "$button_down_command_id" \
  "$button_down_terminal" 'held pointer button before lease expiry'
jq -e '.effect_stage == "button_pressed" and .outcome.type == "acknowledged"' \
  "$button_down_terminal" >/dev/null

button_press_observed=0
for _ in {1..50}; do
  if docker exec "$container_name" sh -eu -c 'cat "$1"' sh "$recorder_events" \
    | jq -se --argjson baseline "$button_event_baseline" '
      any(.[$baseline:][];
        .type == "button_press" and .detail == 1)
    ' >/dev/null; then
    button_press_observed=1
    break
  fi
  sleep 0.1
done
if [[ $button_press_observed -ne 1 ]]; then
  printf 'recorder did not observe the intentionally held physical button\n' >&2
  exit 1
fi

expiry_vacant=0
for _ in {1..300}; do
  expiry_state_body="$test_dir/expiry-lease-state.json"
  expiry_state_headers="$test_dir/expiry-lease-state.headers"
  expiry_state_status=$(authenticated_request GET "/v1/desktops/$desktop_id/lease" '' \
    "$expiry_state_body" "$expiry_state_headers")
  expect_status "$expiry_state_status" 200 "$expiry_state_body" 'expiring lease state'
  expiry_state=$(jq -er '.state' "$expiry_state_body")
  if [[ $expiry_state == vacant ]]; then
    expiry_vacant=1
    break
  fi
  case "$expiry_state" in
    held_by_caller|revoking|resetting) ;;
    *)
      printf 'short lease entered unexpected state during expiry: %s\n' \
        "$expiry_state" >&2
      exit 1
      ;;
  esac
  sleep 0.1
done
if [[ $expiry_vacant -ne 1 ]]; then
  printf 'short lease expiry did not finish its reset transaction\n' >&2
  exit 1
fi

button_release_observed=0
for _ in {1..50}; do
  if docker exec "$container_name" sh -eu -c 'cat "$1"' sh "$recorder_events" \
    | jq -se --argjson baseline "$button_event_baseline" '
      . as $events
      | ([range($baseline; $events | length)
          | select($events[.].type == "button_press" and $events[.].detail == 1)]
          | first) as $press
      | ([range($baseline; $events | length)
          | select($events[.].type == "button_release" and $events[.].detail == 1)]
          | first) as $release
      | $press != null and $release != null and $release > $press
    ' >/dev/null; then
    button_release_observed=1
    break
  fi
  sleep 0.1
done
if [[ $button_release_observed -ne 1 ]]; then
  printf 'lease-expiry reset did not release the held physical button\n' >&2
  exit 1
fi

expired_renew_request="$test_dir/expired-lease-renew.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg desktop_id "$desktop_id" \
  --arg generation "$desktop_generation" --arg lease_id "$short_lease_id" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    desktop_id: $desktop_id, desktop_generation: $generation,
    lease_id: $lease_id, ttl_ms: 60000
  }
' >"$expired_renew_request"
expired_renew_body="$test_dir/expired-lease-renew-response.json"
expired_renew_headers="$test_dir/expired-lease-renew.headers"
expired_renew_status=$(authenticated_request POST \
  "/v1/desktops/$desktop_id/lease/$short_lease_id/renew" "$expired_renew_request" \
  "$expired_renew_body" "$expired_renew_headers")
expect_status "$expired_renew_status" 409 "$expired_renew_body" 'expired lease renewal'
jq -e '.status == 409 and .code == "lease_conflict"' "$expired_renew_body" >/dev/null

reacquire_request="$test_dir/lease-reacquire.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg desktop_id "$desktop_id" \
  --arg generation "$desktop_generation" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    desktop_id: $desktop_id, desktop_generation: $generation, ttl_ms: 60000
  }
' >"$reacquire_request"
reacquire_body="$test_dir/lease-reacquire-response.json"
reacquire_headers="$test_dir/lease-reacquire.headers"
reacquire_status=$(authenticated_request POST "/v1/desktops/$desktop_id/lease" \
  "$reacquire_request" "$reacquire_body" "$reacquire_headers")
expect_status "$reacquire_status" 201 "$reacquire_body" 'post-expiry lease reacquisition'
reacquired_lease_id=$(jq -er '.lease_id' "$reacquire_body")
if [[ $reacquired_lease_id == "$short_lease_id" || $reacquired_lease_id == "$lease_id" ]]; then
  printf 'post-expiry lease reacquisition reused an old capability\n' >&2
  exit 1
fi
reacquire_release_request="$test_dir/lease-reacquire-release.json"
jq -n \
  --arg request_id "$(new_uuid)" --arg desktop_id "$desktop_id" \
  --arg generation "$desktop_generation" --arg lease_id "$reacquired_lease_id" '
  {
    protocol_version: {major: 1, minor: 0}, request_id: $request_id,
    desktop_id: $desktop_id, desktop_generation: $generation, lease_id: $lease_id
  }
' >"$reacquire_release_request"
reacquire_release_body="$test_dir/lease-reacquire-release-response.json"
reacquire_release_headers="$test_dir/lease-reacquire-release.headers"
reacquire_release_status=$(authenticated_request DELETE \
  "/v1/desktops/$desktop_id/lease/$reacquired_lease_id" \
  "$reacquire_release_request" "$reacquire_release_body" \
  "$reacquire_release_headers")
expect_status "$reacquire_release_status" 200 "$reacquire_release_body" \
  'post-expiry reacquired lease release'
jq -e '.state == "vacant" and .lease_id == null and .expires_at == null' \
  "$reacquire_release_body" >/dev/null

docker exec "$container_name" pkill -TERM -u 1000 -f \
  '^/run/xenoteer/phase3-control-plane/x11-event-recorder --focus-before-ready --max-events 512$' \
  >/dev/null 2>&1 || true
nice -n 15 ionice -c 3 "$repo_root/scripts/container/test-phase3-websocket.py" \
  draining --api-base "$api_base" --token-file "$token_file" \
  --desktop-id "$desktop_id" --desktop-generation "$desktop_generation" \
  --container-name "$container_name"
if [[ $(docker inspect "$container_name" --format '{{.State.Running}}') != false ]]; then
  printf 'container remained running after bounded graceful stop\n' >&2
  exit 1
fi
container_exit=$(docker inspect "$container_name" --format '{{.State.ExitCode}}')
if [[ $container_exit -ne 0 ]]; then
  printf 'container graceful stop returned exit code %s\n' "$container_exit" >&2
  exit 1
fi
docker logs "$container_name" >"$test_dir/final-container.log" 2>&1
if grep -Fq -- "$token_canary" "$test_dir/final-container.log"; then
  printf 'container logs exposed the API-token canary\n' >&2
  exit 1
fi
run_restricted_grant_test

printf 'Phase 3 control-plane acceptance passed: %s (%s)\n' \
  "$image_reference" "$image"
