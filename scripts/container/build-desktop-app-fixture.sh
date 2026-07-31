#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
base_image=${XENOTEER_IMAGE:-xenoteer:dev}
fixture_image=${XENOTEER_DESKTOP_APPS_IMAGE:-xenoteer:desktop-apps-test}
artifact_lock=$repo_root/container/fixtures/desktop-apps/artifacts.lock
. "$repo_root/scripts/container/local-image-build-reference.sh"

cleanup() {
  local original_status=$? alias_cleanup_status
  trap - EXIT HUP INT TERM
  set +e
  xenoteer_stop_guarded_local_image_command
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

xenoteer_create_local_image_alias "$base_image" desktop-fixture
base_image_id=$XENOTEER_LOCAL_IMAGE_ID
base_build_reference=$XENOTEER_LOCAL_IMAGE_ALIAS
xenoteer_verify_local_image_alias

lock_value() {
  local key=$1 value
  value=$(sed -n "s/^${key}=//p" "$artifact_lock")
  if [[ -z $value ]] || [[ $(grep -c "^${key}=" "$artifact_lock") -ne 1 ]]; then
    printf 'artifact lock must contain exactly one non-empty %s entry\n' "$key" >&2
    exit 1
  fi
  printf '%s' "$value"
}

electron_version=$(lock_value ELECTRON_VERSION)
electron_url=$(lock_value ELECTRON_LINUX_X64_URL)
electron_sha256=$(lock_value ELECTRON_LINUX_X64_SHA256)

printf 'building desktop fixture from immutable base %s (resolved from %s)\n' \
  "$base_image_id" "$base_image"
xenoteer_prepare_local_image_iidfile
xenoteer_run_guarded_local_image_command docker build \
  "$@" \
  --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
  --file "$repo_root/container/fixtures/desktop-apps/Dockerfile" \
  --build-arg "XENOTEER_BASE_IMAGE=$base_build_reference" \
  --build-arg "XENOTEER_FIXTURE_BASE_IMAGE_ID=$base_image_id" \
  --build-arg "ELECTRON_VERSION=$electron_version" \
  --build-arg "ELECTRON_LINUX_X64_URL=$electron_url" \
  --build-arg "ELECTRON_LINUX_X64_SHA256=$electron_sha256" \
  --label com.aeor.xenoteer.distribution-scope=test-only-non-distributable \
  --tag "$fixture_image" \
  "$repo_root"

xenoteer_verify_local_image_alias

xenoteer_verify_local_image_derivation "$fixture_image"
fixture_image_id=$XENOTEER_LOCAL_DERIVED_IMAGE_ID

recorded_base_id=$(docker image inspect "$fixture_image_id" \
  --format '{{index .Config.Labels "com.aeor.xenoteer.fixture.base-image-id"}}')
recorded_electron_sha=$(docker image inspect "$fixture_image_id" \
  --format '{{index .Config.Labels "com.aeor.xenoteer.fixture.electron-linux-x64-sha256"}}')
test "$recorded_base_id" = "$base_image_id"
test "$recorded_electron_sha" = "$electron_sha256"

printf 'desktop fixture image %s records exact base %s\n' \
  "$fixture_image_id" "$base_image_id"
