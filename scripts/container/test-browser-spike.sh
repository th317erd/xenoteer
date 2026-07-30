#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

runtime_image=${1:-xenoteer:phase0}
spike_image=${2:-xenoteer:browser-spike}
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
seccomp_profile="$repo_root/container/spikes/browser/seccomp_profile.json"
token_file=$(mktemp)
. "$repo_root/scripts/container/local-image-build-reference.sh"
containers=()
cleanup() {
  local original_status=$? alias_cleanup_status name
  trap - EXIT HUP INT TERM
  set +e
  xenoteer_stop_guarded_local_image_command
  for name in "${containers[@]}"; do
    docker rm --force --volumes "$name" >/dev/null 2>&1 || true
  done
  rm -f -- "$token_file"
  xenoteer_cleanup_local_image_alias
  alias_cleanup_status=$?
  if [[ $original_status -ne 0 ]]; then
    exit "$original_status"
  fi
  exit "$alias_cleanup_status"
}
signal_exit() {
  local signal_status=$1
  trap - HUP INT TERM
  set +e
  xenoteer_stop_guarded_local_image_command
  exit "$signal_status"
}
trap cleanup EXIT
trap 'signal_exit 129' HUP
trap 'signal_exit 130' INT
trap 'signal_exit 143' TERM

[[ $(id -u) -eq 0 ]] || { printf 'browser spike must run as root for deterministic secret ownership\n' >&2; exit 77; }
openssl rand -hex 32 >"$token_file"
chmod 0400 "$token_file"
chown 0:0 "$token_file"

xenoteer_create_local_image_alias "$runtime_image" browser
xenoteer_verify_local_image_alias

xenoteer_prepare_local_image_iidfile
xenoteer_run_guarded_local_image_command docker build \
  --build-arg "XENOTEER_RUNTIME_IMAGE=$XENOTEER_LOCAL_IMAGE_ALIAS" \
  --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
  --file "$repo_root/container/spikes/browser/Dockerfile" \
  --tag "$spike_image" \
  "$repo_root"

xenoteer_verify_local_image_alias

xenoteer_verify_local_image_derivation
verified_spike_image_id=$XENOTEER_LOCAL_DERIVED_IMAGE_ID

"$repo_root/scripts/container/test-browser-seccomp.sh"

run_profile() {
  local profile=$1
  local name="xenoteer-browser-${profile}-$$"
  shift
  containers+=("$name")
  xenoteer_run_guarded_local_image_command docker run --detach \
    --name "$name" \
    --shm-size 4g \
    --security-opt "seccomp=$seccomp_profile" \
    --volume "$token_file:/run/secrets/xenoteer_api_token:ro" \
    "$@" \
    "$verified_spike_image_id" >/dev/null
  for _ in {1..60}; do
    xenoteer_run_guarded_local_image_command \
      docker exec "$name" /usr/local/libexec/xenoteer/healthcheck \
      >/dev/null 2>&1 && break
    if [[ $(docker inspect "$name" --format '{{.State.Running}}') != true ]]; then
      xenoteer_run_guarded_local_image_command docker logs "$name" >&2
      return 1
    fi
    sleep 1
  done
  xenoteer_run_guarded_local_image_command \
    docker exec "$name" /usr/local/libexec/xenoteer/healthcheck >/dev/null

  local -a desktop_exec=(
    docker exec
    --user 1000:1000
    --env DISPLAY=:99
    --env XAUTHORITY=/run/user/1000/Xauthority
    --env HOME=/home/xenoteer
    --env XDG_RUNTIME_DIR=/run/user/1000
    "$name"
  )
  xenoteer_run_guarded_local_image_command \
    "${desktop_exec[@]}" /opt/xenoteer-spikes/browser/run-chromium-spike
  xenoteer_run_guarded_local_image_command \
    "${desktop_exec[@]}" /opt/xenoteer-spikes/browser/run-qtwebengine-spike

  # Expansion belongs to the container shell.
  # shellcheck disable=SC2016
  xenoteer_run_guarded_local_image_command docker exec "$name" sh -eu -c '
    test "$(stat -c "%a %u:%g" /usr/lib/chromium/chrome-sandbox)" = "4755 0:0"
    grep -Eq "^Seccomp:[[:space:]]+2$" /proc/1/status
    test "$(stat -c %u /tmp/xenoteer-chromium-spike.png)" = 1000
    test "$(stat -c %u /tmp/xenoteer-qtwebengine-spike.png)" = 1000
    test "$(cat /run/xenoteer/shm-bytes)" -ge 4294967296
    test -s /usr/share/doc/xenoteer/browser-spike-package-manifest.tsv
  '
  xenoteer_run_guarded_local_image_command \
    "${desktop_exec[@]}" /usr/bin/unshare --user --map-root-user true
  xenoteer_run_guarded_local_image_command \
    "${desktop_exec[@]}" /usr/bin/python3 -c '
import ctypes
import errno

libc = ctypes.CDLL(None, use_errno=True)
result = libc.syscall(250, 0, 0, 0, 0, 0)
observed = ctypes.get_errno()
if result != -1 or observed != errno.EPERM:
    raise SystemExit(f"keyctl was not denied: result={result} errno={observed}")
'
  xenoteer_run_guarded_local_image_command \
    docker stop --time 35 "$name" >/dev/null
  printf 'browser spike profile passed: %s\n' "$profile"
}

run_profile default
run_profile hardened \
  --read-only \
  --cap-drop ALL \
  --cap-add CHOWN \
  --cap-add DAC_OVERRIDE \
  --cap-add FOWNER \
  --cap-add KILL \
  --cap-add SETGID \
  --cap-add SETUID \
  --cap-add SYS_CHROOT \
  --security-opt no-new-privileges:true \
  --pids-limit 512 \
  --memory 6g \
  --cpus 2 \
  --tmpfs /run:rw,nosuid,nodev,exec,size=64m,mode=0755 \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=1g,mode=1777 \
  --volume /home/xenoteer \
  --volume /workspace

printf 'browser sandbox spike passed in default and hardened profiles\n'
