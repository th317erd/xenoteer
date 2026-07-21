#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck disable=SC1091
source "$repo_root/container/locks/release.lock"
"$repo_root/scripts/container/validate-locks.sh"

platform=${XENOTEER_PLATFORM:-linux/amd64}
if [[ $platform != "$SUPPORTED_PLATFORM" ]]; then
  printf 'unsupported platform %s; Phase 0 supports only %s\n' "$platform" "$SUPPORTED_PLATFORM" >&2
  exit 64
fi

revision=${XENOTEER_REVISION:-$(git -C "$repo_root" rev-parse --verify HEAD)}
version=${XENOTEER_VERSION:-0.1.0-dev}
created=$(date --utc --date="@${SOURCE_DATE_EPOCH:-0}" '+%Y-%m-%dT%H:%M:%SZ')

exec docker build \
  --platform "$platform" \
  --build-arg "DEBIAN_BASE_IMAGE=${DEBIAN_BASE_TAG}@${DEBIAN_BASE_DIGEST}" \
  --build-arg "DEBIAN_BASE_DIGEST=${DEBIAN_BASE_DIGEST}" \
  --build-arg "DEBIAN_SNAPSHOT=${DEBIAN_SNAPSHOT}" \
  --build-arg "RUST_BUILDER_IMAGE=${RUST_BUILDER_TAG}@${RUST_BUILDER_DIGEST}" \
  --build-arg "RUST_BUILDER_DEBIAN_SUITE=${RUST_BUILDER_DEBIAN_SUITE}" \
  --build-arg "S6_OVERLAY_VERSION=${S6_OVERLAY_VERSION}" \
  --build-arg "S6_OVERLAY_NOARCH_SHA256=${S6_OVERLAY_NOARCH_SHA256}" \
  --build-arg "S6_OVERLAY_ARCH=x86_64" \
  --build-arg "S6_OVERLAY_ARCH_SHA256=${S6_OVERLAY_X86_64_SHA256}" \
  --build-arg "S6_OVERLAY_COPYING_SHA256=${S6_OVERLAY_COPYING_SHA256}" \
  --build-arg "XENOTEER_VERSION=${version}" \
  --build-arg "XENOTEER_REVISION=${revision}" \
  --build-arg "XENOTEER_CREATED=${created}" \
  --tag "${XENOTEER_IMAGE:-xenoteer:dev}" \
  "$@" \
  "$repo_root"
