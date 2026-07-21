#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck disable=SC1091
source "$repo_root/container/locks/release.lock"
"$repo_root/scripts/container/validate-locks.sh"

platform=${XENOTEER_PLATFORM:-linux/amd64}
if [[ $platform != "$SUPPORTED_PLATFORM" ]]; then
  printf 'unsupported platform %s; this release supports only %s\n' "$platform" "$SUPPORTED_PLATFORM" >&2
  exit 64
fi

head_revision=$(git -C "$repo_root" rev-parse --verify HEAD)
if [[ -n ${XENOTEER_REVISION:-} && $XENOTEER_REVISION != "$head_revision" ]]; then
  printf 'XENOTEER_REVISION must equal the checked-out commit %s\n' "$head_revision" >&2
  exit 64
fi

source_tree_hash() {
  {
    printf 'HEAD\0%s\0' "$head_revision"
    git -C "$repo_root" diff --binary --no-ext-diff HEAD --
    while IFS= read -r -d '' path; do
      [[ $path != *$'\n'* && $path != *$'\t'* ]] || {
        printf 'unsupported untracked source path: %q\n' "$path" >&2
        exit 1
      }
      printf 'untracked\0%s\0%s\0%s\0' \
        "$path" \
        "$(stat -c '%a' "$repo_root/$path")" \
        "$(sha256sum "$repo_root/$path" | awk '{print $1}')"
    done < <(git -C "$repo_root" ls-files --others --exclude-standard -z)
  } | sha256sum | awk '{print $1}'
}

source_tree_sha256=$(source_tree_hash)

source_dirty=false
revision=$head_revision
if [[ -n $(git -C "$repo_root" status --porcelain=v1 --untracked-files=all) ]]; then
  source_dirty=true
  revision="${head_revision}-dirty.${source_tree_sha256:0:12}"
fi

dependency_lock_sha256=$(
  "$repo_root/scripts/container/dependency-lock-hash.sh"
)
version=${XENOTEER_VERSION:-0.1.0-dev}
created=$(date --utc --date="@${SOURCE_DATE_EPOCH:-0}" '+%Y-%m-%dT%H:%M:%SZ')
image=${XENOTEER_IMAGE:-xenoteer:dev}

DOCKER_BUILDKIT=1 docker build \
  --platform "$platform" \
  --build-arg "DEBIAN_BASE_IMAGE=${DEBIAN_BASE_TAG}@${DEBIAN_BASE_DIGEST}" \
  --build-arg "DEBIAN_BASE_DIGEST=${DEBIAN_BASE_DIGEST}" \
  --build-arg "DEBIAN_SNAPSHOT=${DEBIAN_SNAPSHOT}" \
  --build-arg "DEBIAN_SUITE=${DEBIAN_SUITE}" \
  --build-arg "DEBIAN_INRELEASE_SHA256=${DEBIAN_INRELEASE_SHA256}" \
  --build-arg "DEBIAN_UPDATES_INRELEASE_SHA256=${DEBIAN_UPDATES_INRELEASE_SHA256}" \
  --build-arg "DEBIAN_SECURITY_INRELEASE_SHA256=${DEBIAN_SECURITY_INRELEASE_SHA256}" \
  --build-arg "RUST_BUILDER_IMAGE=${RUST_BUILDER_TAG}@${RUST_BUILDER_DIGEST}" \
  --build-arg "RUST_BUILDER_DEBIAN_SUITE=${RUST_BUILDER_DEBIAN_SUITE}" \
  --build-arg "RUST_BUILDER_DEBIAN_INRELEASE_SHA256=${RUST_BUILDER_DEBIAN_INRELEASE_SHA256}" \
  --build-arg "RUST_BUILDER_DEBIAN_UPDATES_INRELEASE_SHA256=${RUST_BUILDER_DEBIAN_UPDATES_INRELEASE_SHA256}" \
  --build-arg "RUST_BUILDER_DEBIAN_SECURITY_INRELEASE_SHA256=${RUST_BUILDER_DEBIAN_SECURITY_INRELEASE_SHA256}" \
  --build-arg "S6_OVERLAY_VERSION=${S6_OVERLAY_VERSION}" \
  --build-arg "S6_OVERLAY_NOARCH_SHA256=${S6_OVERLAY_NOARCH_SHA256}" \
  --build-arg "S6_OVERLAY_ARCH=x86_64" \
  --build-arg "S6_OVERLAY_ARCH_SHA256=${S6_OVERLAY_X86_64_SHA256}" \
  --build-arg "S6_OVERLAY_COPYING_SHA256=${S6_OVERLAY_COPYING_SHA256}" \
  --build-arg "NOVNC_VERSION=${NOVNC_VERSION}" \
  --build-arg "NOVNC_DEB_SHA256=${NOVNC_DEB_SHA256}" \
  --build-arg "PYTHON3_WEBSOCKIFY_VERSION=${PYTHON3_WEBSOCKIFY_VERSION}" \
  --build-arg "PYTHON3_WEBSOCKIFY_DEB_SHA256=${PYTHON3_WEBSOCKIFY_DEB_SHA256}" \
  --build-arg "TIGERVNC_SCRAPING_SERVER_VERSION=${TIGERVNC_SCRAPING_SERVER_VERSION}" \
  --build-arg "TIGERVNC_SCRAPING_SERVER_DEB_SHA256=${TIGERVNC_SCRAPING_SERVER_DEB_SHA256}" \
  --build-arg "XENOTEER_VERSION=${version}" \
  --build-arg "XENOTEER_REVISION=${revision}" \
  --build-arg "XENOTEER_CREATED=${created}" \
  --build-arg "XENOTEER_SOURCE_DIRTY=${source_dirty}" \
  --build-arg "XENOTEER_SOURCE_TREE_SHA256=${source_tree_sha256}" \
  --build-arg "XENOTEER_DEPENDENCY_LOCK_SHA256=${dependency_lock_sha256}" \
  --tag "$image" \
  "$@" \
  "$repo_root"

after_build_source_tree_sha256=$(source_tree_hash)
if [[ $after_build_source_tree_sha256 != "$source_tree_sha256" ]]; then
  printf 'source tree changed while Docker captured/built the context; do not accept tag %s\n' \
    "$image" >&2
  exit 75
fi

assert_label() {
  local label=$1 expected=$2 actual
  actual=$(docker image inspect "$image" --format "{{index .Config.Labels \"$label\"}}")
  if [[ $actual != "$expected" ]]; then
    printf 'built image label %s differs from the wrapper input\n' "$label" >&2
    exit 1
  fi
}

assert_label org.opencontainers.image.revision "$revision"
assert_label com.aeor.xenoteer.source.dirty "$source_dirty"
assert_label com.aeor.xenoteer.source-tree.sha256 "$source_tree_sha256"
assert_label com.aeor.xenoteer.dependency-lock.sha256 "$dependency_lock_sha256"

printf 'built %s from source tree %s (dirty=%s; dependency locks=%s)\n' \
  "$image" "$source_tree_sha256" "$source_dirty" "$dependency_lock_sha256"
