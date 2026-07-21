#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
base_image=${XENOTEER_IMAGE:-xenoteer:dev}
fixture_image=${XENOTEER_DESKTOP_APPS_IMAGE:-xenoteer:desktop-apps-test}
artifact_lock=$repo_root/container/fixtures/desktop-apps/artifacts.lock

base_image_id=$(docker image inspect "$base_image" --format '{{.Id}}')
if [[ ! $base_image_id =~ ^sha256:[0-9a-f]{64}$ ]]; then
  printf 'base image did not resolve to an immutable image ID: %s\n' "$base_image" >&2
  exit 1
fi
base_build_reference="xenoteer-fixture-base-$$:${base_image_id#sha256:}"
docker image tag "$base_image_id" "$base_build_reference"
cleanup() {
  docker image rm "$base_build_reference" >/dev/null 2>&1 || true
}
trap cleanup EXIT

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
docker build \
  "$@" \
  --file "$repo_root/container/fixtures/desktop-apps/Dockerfile" \
  --build-arg "XENOTEER_BASE_IMAGE=$base_build_reference" \
  --build-arg "XENOTEER_FIXTURE_BASE_IMAGE_ID=$base_image_id" \
  --build-arg "ELECTRON_VERSION=$electron_version" \
  --build-arg "ELECTRON_LINUX_X64_URL=$electron_url" \
  --build-arg "ELECTRON_LINUX_X64_SHA256=$electron_sha256" \
  --label com.aeor.xenoteer.distribution-scope=test-only-non-distributable \
  --tag "$fixture_image" \
  "$repo_root"

fixture_image_id=$(docker image inspect "$fixture_image" --format '{{.Id}}')
recorded_base_id=$(docker image inspect "$fixture_image_id" \
  --format '{{index .Config.Labels "com.aeor.xenoteer.fixture.base-image-id"}}')
recorded_electron_sha=$(docker image inspect "$fixture_image_id" \
  --format '{{index .Config.Labels "com.aeor.xenoteer.fixture.electron-linux-x64-sha256"}}')
test "$recorded_base_id" = "$base_image_id"
test "$recorded_electron_sha" = "$electron_sha256"

docker image inspect "$base_image_id" "$fixture_image_id" \
  | python3 -c '
import json
import sys

base, fixture = json.load(sys.stdin)
base_layers = base["RootFS"]["Layers"]
fixture_layers = fixture["RootFS"]["Layers"]
if fixture_layers[: len(base_layers)] != base_layers:
    raise SystemExit("derived fixture does not have the recorded exact base layer prefix")
if fixture["Id"] != sys.argv[2] or base["Id"] != sys.argv[1]:
    raise SystemExit("image identity changed during fixture build verification")
' "$base_image_id" "$fixture_image_id"

printf 'desktop fixture image %s records exact base %s\n' \
  "$fixture_image_id" "$base_image_id"
