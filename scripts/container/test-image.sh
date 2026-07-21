#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

image=${1:-xenoteer:dev}
container_name="xenoteer-image-test-$$"
token_file=$(mktemp)
token_canary='PHASE0_IMAGE_TEST_TOKEN_VALUE_MUST_NEVER_APPEAR_IN_LOGS_0123456789'
created=()

cleanup() {
  local name
  for name in "${created[@]}"; do
    docker rm --force --volumes "$name" >/dev/null 2>&1 || true
  done
  rm -f -- "$token_file"
}
trap cleanup EXIT

printf '%s' "$token_canary" >"$token_file"
chmod 0400 "$token_file"
if [[ $(id -u) -eq 0 ]]; then
  chown 1000:1000 "$token_file"
elif [[ $(id -u) -ne 1000 ]]; then
  printf 'test-image must run as root or UID 1000 so the secret owner is deterministic\n' >&2
  exit 77
fi

start_container() {
  local name=$1
  shift
  docker run --detach \
    --name "$name" \
    --shm-size=4g \
    --volume "$token_file:/run/secrets/xenoteer_api_token:ro" \
    "$@" \
    "$image" >/dev/null
  created+=("$name")
}

wait_running_probe() {
  local name=$1
  for _ in {1..45}; do
    if docker exec "$name" /usr/local/libexec/xenoteer/healthcheck >/dev/null 2>&1; then
      return 0
    fi
    if [[ $(docker inspect "$name" --format '{{.State.Running}}') != true ]]; then
      docker logs "$name" >&2
      return 1
    fi
    sleep 1
  done
  docker logs "$name" >&2
  printf '%s did not pass the Phase-0 liveness probe\n' "$name" >&2
  return 1
}

wait_stopped() {
  local name=$1
  # The critical finish hook requests an overlay halt and exits 125 to prevent a
  # respawn race. Keep a bounded margin for the outer s6-rc shutdown transaction.
  for _ in {1..50}; do
    [[ $(docker inspect "$name" --format '{{.State.Running}}') == false ]] && return 0
    sleep 1
  done
  docker logs "$name" >&2
  printf '%s did not stop after critical service exit\n' "$name" >&2
  return 1
}

assert_logs_exclude() {
  local name=$1 forbidden=$2 description=$3
  if docker logs "$name" 2>&1 | grep -Fq -- "$forbidden"; then
    printf '%s logs exposed %s\n' "$name" "$description" >&2
    exit 1
  fi
}

# Stop the image immediately after detach, before Docker can report readiness.
# This exercises a requested down transition while the s6 startup transaction
# may still be in flight; wantedup=false must classify it as orderly.
startup_stop="${container_name}-startup-stop"
start_container "$startup_stop"
test "$(docker inspect "$startup_stop" --format '{{.State.Health.Status}}')" = starting
docker stop --time 35 "$startup_stop" >/dev/null
test "$(docker inspect "$startup_stop" --format '{{.State.ExitCode}}')" -eq 0
if docker logs "$startup_stop" 2>&1 | grep -Fq 'exited unexpectedly'; then
  printf 'immediate startup stop was classified as a critical crash\n' >&2
  exit 1
fi

start_container "$container_name"
wait_running_probe "$container_name"

test "$(docker inspect "$container_name" --format '{{json .Config.Entrypoint}}')" = '["/init"]'
test "$(docker exec "$container_name" cat /proc/1/comm)" = s6-svscan
docker exec "$container_name" sh -eu -c '
  test "$(stat -c %u /proc/$(pgrep -xo Xvfb))" = 1000
  test "$(stat -c %u /proc/$(pgrep -xo xenoteerd))" = 1000
  test "$(stat -c %a /run/user/1000)" = 700
  test "$(stat -c %a /run/user/1000/Xauthority)" = 600
  test "$(stat -c %a /tmp/.X11-unix)" = 1777
  test "$(cat /run/xenoteer/shm-bytes)" -ge 4294967296
  test "$(cat /run/xenoteer/env/XVFB_SCREEN_GEOMETRY)" = 1920x1080x24
  DISPLAY=:99 XAUTHORITY=/run/user/1000/Xauthority xdpyinfo \
    | grep -F "dimensions:    1920x1080 pixels" >/dev/null
  DISPLAY=:99 XAUTHORITY=/run/user/1000/Xauthority xdpyinfo \
    | grep -F "resolution:    96x96 dots per inch" >/dev/null
  ! env -u XAUTHORITY HOME=/tmp xdpyinfo -display :99 >/dev/null 2>&1
  ! awk "NR > 1 && \$2 ~ /:17D3$/ { found=1 } END { exit found ? 0 : 1 }" /proc/net/tcp /proc/net/tcp6
  curl --fail --silent --show-error http://127.0.0.1:8080/livez >/dev/null
  test "$(curl --silent --output /dev/null --write-out "%{http_code}" http://127.0.0.1:8080/readyz)" = 503
  tail -n +2 /usr/share/doc/xenoteer/package-manifest.tsv | LC_ALL=C sort -c
  test -s /usr/share/doc/xenoteer/first-party-files.tsv
  test -s /usr/share/doc/xenoteer/final-files.tsv
  test -s /usr/share/doc/xenoteer/cargo-components.tsv
  test -s /usr/share/doc/xenoteer/cargo-components.spdx.json
  grep -Eq "^xenoteerd[[:space:]]" /usr/share/doc/xenoteer/cargo-components.tsv
  grep -Eq "^tokio[[:space:]]" /usr/share/doc/xenoteer/cargo-components.tsv
  grep -Eq "^/init[[:space:]].*[[:space:]]locked-third-party[[:space:]]s6-overlay-3\\.2\\.2\\.0[[:space:]]ISC[[:space:]]" \
    /usr/share/doc/xenoteer/final-files.tsv
  grep -Eq "^/usr/share/doc/xenoteer/cargo-components\\.tsv[[:space:]].*[[:space:]]generated-metadata[[:space:]]xenoteerd-cargo-closure[[:space:]]" \
    /usr/share/doc/xenoteer/final-files.tsv
'
docker exec "$container_name" cat /usr/share/doc/xenoteer/cargo-components.spdx.json \
  | jq -e '.spdxVersion == "SPDX-2.3" and ([.packages[].name] | index("xenoteerd") != null)' \
  >/dev/null
assert_logs_exclude "$container_name" "$token_canary" 'authentication token contents'

for _ in {1..45}; do
  health=$(docker inspect "$container_name" --format '{{.State.Health.Status}}')
  [[ $health == healthy ]] && break
  sleep 1
done
test "$(docker inspect "$container_name" --format '{{.State.Health.Status}}')" = healthy

started=$(date +%s)
docker stop --time 35 "$container_name" >/dev/null
elapsed=$(($(date +%s) - started))
if (( elapsed > 35 )); then
  printf 'clean stop exceeded 35 seconds: %ss\n' "$elapsed" >&2
  exit 1
fi
test "$(docker inspect "$container_name" --format '{{.State.ExitCode}}')" -eq 0
if docker logs "$container_name" 2>&1 | grep -Fq 'exited unexpectedly'; then
  printf 'healthy operator stop was classified as a critical crash\n' >&2
  exit 1
fi

hardened="${container_name}-hardened"
hardened_args=(
  --read-only
  --cap-drop ALL
  --cap-add CHOWN
  --cap-add DAC_OVERRIDE
  --cap-add FOWNER
  --cap-add KILL
  --cap-add SETGID
  --cap-add SETUID
  --cap-add SYS_CHROOT
  --security-opt no-new-privileges:true
  --pids-limit 512
  --memory 6g
  --cpus 2
  --tmpfs '/run:rw,nosuid,nodev,exec,size=64m,mode=0755'
  --tmpfs '/tmp:rw,nosuid,nodev,noexec,size=1g,mode=1777'
  --volume /home/xenoteer
  --volume /workspace
)
start_container "$hardened" "${hardened_args[@]}"
wait_running_probe "$hardened"
test "$(docker inspect "$hardened" --format '{{.HostConfig.ReadonlyRootfs}}')" = true
test "$(docker inspect "$hardened" --format '{{index .HostConfig.SecurityOpt 0}}')" = no-new-privileges:true
docker inspect "$hardened" --format '{{json .HostConfig.CapAdd}}' \
  | jq -e 'sort == ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_KILL", "CAP_SETGID", "CAP_SETUID", "CAP_SYS_CHROOT"]' \
  >/dev/null
docker stop --time 35 "$hardened" >/dev/null
test "$(docker inspect "$hardened" --format '{{.State.ExitCode}}')" -eq 0
assert_logs_exclude "$hardened" "$token_canary" 'authentication token contents'
if docker logs "$hardened" 2>&1 | grep -Fq 'exited unexpectedly'; then
  printf 'healthy hardened stop was classified as a critical crash\n' >&2
  exit 1
fi

# Negative capability proof: derive the otherwise exact hardened profile by
# deleting only CAP_KILL. PID 1 can still boot, but cannot gracefully signal the
# UID-1000 payloads, so Docker must enforce the short stop deadline with SIGKILL.
hardened_without_kill_args=()
for ((i = 0; i < ${#hardened_args[@]}; i++)); do
  if [[ ${hardened_args[i]} == --cap-add && ${hardened_args[i + 1]:-} == KILL ]]; then
    i=$((i + 1))
    continue
  fi
  hardened_without_kill_args+=("${hardened_args[i]}")
done
without_kill="${container_name}-hardened-without-kill"
start_container "$without_kill" "${hardened_without_kill_args[@]}"
wait_running_probe "$without_kill"
docker inspect "$without_kill" --format '{{json .HostConfig.CapAdd}}' \
  | jq -e 'sort == ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_SETGID", "CAP_SETUID", "CAP_SYS_CHROOT"]' \
  >/dev/null
docker exec "$without_kill" sh -eu -c '
  test "$(stat -c %u /proc/$(pgrep -xo Xvfb))" = 1000
  test "$(stat -c %u /proc/$(pgrep -xo xenoteerd))" = 1000
'
started_ms=$(date +%s%3N)
docker stop --time 5 "$without_kill" >/dev/null
elapsed_ms=$(($(date +%s%3N) - started_ms))
test "$(docker inspect "$without_kill" --format '{{.State.ExitCode}}')" -eq 137
if (( elapsed_ms > 12000 )); then
  printf 'CAP_KILL negative stop proof exceeded its bound: %sms\n' "$elapsed_ms" >&2
  exit 1
fi

logging="${container_name}-logging"
logging_filter='info,xenoteerd=trace'
start_container "$logging" --env "XENOTEER__LOGGING__FILTER=$logging_filter"
wait_running_probe "$logging"
logging_output=$(docker logs "$logging" 2>&1)
grep -Fq 'loaded validated configuration' <<<"$logging_output"
grep -Fq "$logging_filter" <<<"$logging_output"
assert_logs_exclude "$logging" "$token_canary" 'authentication token contents'
docker stop --time 35 "$logging" >/dev/null

malformed="${container_name}-malformed-env"
malformed_value='PHASE0_MALFORMED_ENV_VALUE_MUST_NOT_LEAK'
start_container "$malformed" --env "XENOTEER_BAD=$malformed_value"
wait_stopped "$malformed"
test "$(docker inspect "$malformed" --format '{{.State.ExitCode}}')" -ne 0
malformed_output=$(docker logs "$malformed" 2>&1)
grep -Fq 'invalid Xenoteer environment configuration key' <<<"$malformed_output"
assert_logs_exclude "$malformed" "$malformed_value" 'malformed environment value'
assert_logs_exclude "$malformed" "$token_canary" 'authentication token contents'

strict_loader="${container_name}-strict-loader"
strict_value='PHASE0_TYPED_UNKNOWN_VALUE_MUST_NOT_LEAK'
start_container "$strict_loader" --env "XENOTEER__UNKNOWN__FIELD=$strict_value"
wait_stopped "$strict_loader"
test "$(docker inspect "$strict_loader" --format '{{.State.ExitCode}}')" -ne 0
strict_output=$(docker logs "$strict_loader" 2>&1)
grep -Fq 'xenoteerd startup failed: configuration shape is invalid: unknown field' \
  <<<"$strict_output"
test "$(grep -Fc 'xenoteerd startup failed:' <<<"$strict_output")" -eq 1
assert_logs_exclude "$strict_loader" "$strict_value" 'typed environment value'
assert_logs_exclude "$strict_loader" "$token_canary" 'authentication token contents'

strict_hardened="${container_name}-strict-loader-hardened"
start_container "$strict_hardened" "${hardened_args[@]}" \
  --env "XENOTEER__UNKNOWN__FIELD=$strict_value"
wait_stopped "$strict_hardened"
test "$(docker inspect "$strict_hardened" --format '{{.State.ExitCode}}')" -ne 0
strict_hardened_output=$(docker logs "$strict_hardened" 2>&1)
grep -Fq 'xenoteerd startup failed: configuration shape is invalid: unknown field' \
  <<<"$strict_hardened_output"
test "$(grep -Fc 'xenoteerd startup failed:' <<<"$strict_hardened_output")" -eq 1
assert_logs_exclude "$strict_hardened" "$strict_value" 'typed environment value'
assert_logs_exclude "$strict_hardened" "$token_canary" 'authentication token contents'

empty_typed="${container_name}-empty-typed"
start_container "$empty_typed" --env XENOTEER__DESKTOP__DISPLAY_WIDTH=
wait_stopped "$empty_typed"
test "$(docker inspect "$empty_typed" --format '{{.State.ExitCode}}')" -ne 0
empty_output=$(docker logs "$empty_typed" 2>&1)
grep -Fq 'xenoteerd startup failed: configuration shape is invalid: incompatible value type' \
  <<<"$empty_output"
test "$(grep -Fc 'xenoteerd startup failed:' <<<"$empty_output")" -eq 1
assert_logs_exclude "$empty_typed" "$token_canary" 'authentication token contents'

wrong_geometry="${container_name}-wrong-geometry"
start_container "$wrong_geometry" --env XVFB_SCREEN_WIDTH=1280
wait_stopped "$wrong_geometry"
test "$(docker inspect "$wrong_geometry" --format '{{.State.ExitCode}}')" -ne 0
geometry_output=$(docker logs "$wrong_geometry" 2>&1)
grep -Fq 'Phase 0 requires fixed Xvfb geometry 1920x1080x24 at 96 DPI' \
  <<<"$geometry_output"
assert_logs_exclude "$wrong_geometry" "$token_canary" 'authentication token contents'

for critical in Xvfb xenoteerd; do
  name="${container_name}-${critical,,}"
  start_container "$name"
  wait_running_probe "$name"
  docker exec "$name" pkill -TERM -x "$critical"
  wait_stopped "$name"
  test "$(docker inspect "$name" --format '{{.State.ExitCode}}')" -ne 0
  grep -Fq "critical service ${critical,,} exited unexpectedly; container exit result" \
    < <(docker logs "$name" 2>&1)
  assert_logs_exclude "$name" "$token_canary" 'authentication token contents'
done

for critical in Xvfb xenoteerd; do
  name="${container_name}-hardened-${critical,,}"
  start_container "$name" "${hardened_args[@]}"
  wait_running_probe "$name"
  docker exec "$name" pkill -TERM -x "$critical"
  wait_stopped "$name"
  test "$(docker inspect "$name" --format '{{.State.ExitCode}}')" -ne 0
  grep -Fq "critical service ${critical,,} exited unexpectedly; container exit result" \
    < <(docker logs "$name" 2>&1)
  assert_logs_exclude "$name" "$token_canary" 'authentication token contents'
done

no_secret="${container_name}-no-secret"
docker run --detach --name "$no_secret" --shm-size=4g "$image" >/dev/null
created+=("$no_secret")
wait_stopped "$no_secret"
test "$(docker inspect "$no_secret" --format '{{.State.ExitCode}}')" -ne 0

printf 'container image tests passed: %s\n' "$image"
