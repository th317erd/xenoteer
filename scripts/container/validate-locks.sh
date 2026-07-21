#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
release_lock="$repo_root/container/locks/release.lock"
sources_lock="$repo_root/container/locks/sources.lock"
online=0

if [[ ${1:-} == --online ]]; then
  online=1
elif [[ $# -ne 0 ]]; then
  printf 'usage: %s [--online]\n' "$0" >&2
  exit 64
fi

while IFS= read -r line || [[ -n $line ]]; do
  [[ -z $line || $line == \#* ]] && continue
  if [[ ! $line =~ ^[A-Z][A-Z0-9_]*=[A-Za-z0-9._:/@+~-]+$ ]]; then
    printf 'unsafe or malformed release.lock line: %s\n' "$line" >&2
    exit 1
  fi
done <"$release_lock"

# release.lock is accepted as shell only after the grammar check above.
# shellcheck disable=SC1090
source "$release_lock"

required=(
  LOCK_FORMAT_VERSION SUPPORTED_PLATFORM DOCKERFILE_FRONTEND HADOLINT_IMAGE
  DEBIAN_BASE_TAG DEBIAN_BASE_DIGEST DEBIAN_SNAPSHOT DEBIAN_SUITE
  DEBIAN_INRELEASE_SHA256 DEBIAN_UPDATES_INRELEASE_SHA256
  DEBIAN_SECURITY_INRELEASE_SHA256 RUST_BUILDER_TAG RUST_BUILDER_DIGEST
  RUST_BUILDER_DEBIAN_SUITE RUST_BUILDER_DEBIAN_INRELEASE_SHA256
  RUST_BUILDER_DEBIAN_UPDATES_INRELEASE_SHA256
  RUST_BUILDER_DEBIAN_SECURITY_INRELEASE_SHA256 S6_OVERLAY_VERSION
  S6_OVERLAY_NOARCH_SHA256 S6_OVERLAY_X86_64_SHA256 S6_OVERLAY_COPYING_SHA256
  NOVNC_VERSION NOVNC_DEB_SHA256 PYTHON3_WEBSOCKIFY_VERSION
  PYTHON3_WEBSOCKIFY_DEB_SHA256 TIGERVNC_SCRAPING_SERVER_VERSION
  TIGERVNC_SCRAPING_SERVER_DEB_SHA256
)
for name in "${required[@]}"; do
  if [[ -z ${!name:-} ]]; then
    printf 'release.lock is missing %s\n' "$name" >&2
    exit 1
  fi
done

[[ $LOCK_FORMAT_VERSION == 1 ]]
[[ $SUPPORTED_PLATFORM == linux/amd64 ]]
[[ $DOCKERFILE_FRONTEND =~ ^docker/dockerfile:[0-9]+\.[0-9]+\.[0-9]+@sha256:[a-f0-9]{64}$ ]]
[[ $HADOLINT_IMAGE =~ ^hadolint/hadolint:v[0-9]+\.[0-9]+\.[0-9]+-debian@sha256:[a-f0-9]{64}$ ]]
[[ $DEBIAN_BASE_DIGEST =~ ^sha256:[a-f0-9]{64}$ ]]
[[ $RUST_BUILDER_DIGEST =~ ^sha256:[a-f0-9]{64}$ ]]
[[ $DEBIAN_SNAPSHOT =~ ^[0-9]{8}T[0-9]{6}Z$ ]]
[[ $DEBIAN_SUITE == trixie ]]
for metadata_hash in \
  "$DEBIAN_INRELEASE_SHA256" \
  "$DEBIAN_UPDATES_INRELEASE_SHA256" \
  "$DEBIAN_SECURITY_INRELEASE_SHA256" \
  "$RUST_BUILDER_DEBIAN_INRELEASE_SHA256" \
  "$RUST_BUILDER_DEBIAN_UPDATES_INRELEASE_SHA256" \
  "$RUST_BUILDER_DEBIAN_SECURITY_INRELEASE_SHA256"; do
  [[ $metadata_hash =~ ^[a-f0-9]{64}$ ]]
done
[[ $S6_OVERLAY_NOARCH_SHA256 =~ ^[a-f0-9]{64}$ ]]
[[ $S6_OVERLAY_X86_64_SHA256 =~ ^[a-f0-9]{64}$ ]]
[[ $S6_OVERLAY_COPYING_SHA256 =~ ^[a-f0-9]{64}$ ]]
[[ $NOVNC_DEB_SHA256 =~ ^[a-f0-9]{64}$ ]]
[[ $PYTHON3_WEBSOCKIFY_DEB_SHA256 =~ ^[a-f0-9]{64}$ ]]
[[ $TIGERVNC_SCRAPING_SERVER_DEB_SHA256 =~ ^[a-f0-9]{64}$ ]]

if rg -n '(^|[=[:space:]])(latest|edge|main|master|UNRESOLVED|TODO)([=[:space:]]|$)' \
  "$release_lock" "$sources_lock" >/dev/null; then
  printf 'mutable or unresolved release input found in lock files\n' >&2
  exit 1
fi

awk -F '\t' '
  BEGIN { failed = 0 }
  /^#/ || NF == 0 { next }
  NF != 7 {
    printf "sources.lock:%d: expected 7 tab-separated fields, got %d\n", NR, NF > "/dev/stderr"
    failed = 1
    next
  }
  $5 !~ /^[a-f0-9]{64}$/ {
    printf "sources.lock:%d: invalid sha256\n", NR > "/dev/stderr"
    failed = 1
  }
  seen[$1]++ {
    printf "sources.lock:%d: duplicate component %s\n", NR, $1 > "/dev/stderr"
    failed = 1
  }
  END { exit failed }
' "$sources_lock"

test "$(head -n 1 "$repo_root/Dockerfile")" = "# syntax=$DOCKERFILE_FRONTEND"
test "$(awk -F '\t' '$1 == "dockerfile-frontend" { print $5 }' "$sources_lock")" = \
  "${DOCKERFILE_FRONTEND##*@sha256:}"
test "$(awk -F '\t' '$1 == "hadolint" { print $5 }' "$sources_lock")" = \
  "${HADOLINT_IMAGE##*@sha256:}"
test "$(awk -F '\t' '$1 == "debian-base" { print $5 }' "$sources_lock")" = "${DEBIAN_BASE_DIGEST#sha256:}"
test "$(awk -F '\t' '$1 == "rust-builder" { print $5 }' "$sources_lock")" = "${RUST_BUILDER_DIGEST#sha256:}"
test "$(awk -F '\t' '$1 == "debian-inrelease-trixie" { print $5 }' "$sources_lock")" = \
  "$DEBIAN_INRELEASE_SHA256"
test "$(awk -F '\t' '$1 == "debian-inrelease-trixie-updates" { print $5 }' "$sources_lock")" = \
  "$DEBIAN_UPDATES_INRELEASE_SHA256"
test "$(awk -F '\t' '$1 == "debian-security-inrelease-trixie" { print $5 }' "$sources_lock")" = \
  "$DEBIAN_SECURITY_INRELEASE_SHA256"
test "$(awk -F '\t' '$1 == "rust-builder-debian-inrelease" { print $5 }' "$sources_lock")" = \
  "$RUST_BUILDER_DEBIAN_INRELEASE_SHA256"
test "$(awk -F '\t' '$1 == "rust-builder-debian-updates-inrelease" { print $5 }' "$sources_lock")" = \
  "$RUST_BUILDER_DEBIAN_UPDATES_INRELEASE_SHA256"
test "$(awk -F '\t' '$1 == "rust-builder-debian-security-inrelease" { print $5 }' "$sources_lock")" = \
  "$RUST_BUILDER_DEBIAN_SECURITY_INRELEASE_SHA256"
test "$(awk -F '\t' '$1 == "s6-overlay-noarch" { print $5 }' "$sources_lock")" = "$S6_OVERLAY_NOARCH_SHA256"
test "$(awk -F '\t' '$1 == "s6-overlay-x86_64" { print $5 }' "$sources_lock")" = "$S6_OVERLAY_X86_64_SHA256"
test "$(awk -F '\t' '$1 == "s6-overlay-copying" { print $5 }' "$sources_lock")" = "$S6_OVERLAY_COPYING_SHA256"
test "$(awk -F '\t' '$1 == "novnc-debian-package" { print $2 ":" $5 }' "$sources_lock")" = \
  "$NOVNC_VERSION:$NOVNC_DEB_SHA256"
test "$(awk -F '\t' '$1 == "python3-websockify-debian-package" { print $2 ":" $5 }' "$sources_lock")" = \
  "$PYTHON3_WEBSOCKIFY_VERSION:$PYTHON3_WEBSOCKIFY_DEB_SHA256"
test "$(awk -F '\t' '$1 == "tigervnc-scraping-server-debian-package" { print $2 ":" $5 }' "$sources_lock")" = \
  "$TIGERVNC_SCRAPING_SERVER_VERSION:$TIGERVNC_SCRAPING_SERVER_DEB_SHA256"
test "$(awk -F '\t' '$1 == "moby-docker-default-seccomp" { print $2 ":" $5 }' "$sources_lock")" = \
  'docker-v29.1.3@fbf3ed25f893e6ce21336f1101590e40a13934f4:01536f1d1df938ae611eba20d6349e0de7a99b6ecdee1549427a0b01b8301e28'
test "$(awk -F '\t' '$1 == "playwright-browser-seccomp-rule" { print $2 ":" $5 }' "$sources_lock")" = \
  'ae935a43d9e376e4759548f6b3c6905c7b282333:cc3e61cabda6bbc1e53e54d27ba4d55a9d3be829b6dd1a596f4a7b31b1cc7849'
test "$(awk -F '\t' '$1 == "moby-license" { print $2 ":" $5 }' "$sources_lock")" = \
  'docker-v29.1.3@fbf3ed25f893e6ce21336f1101590e40a13934f4:7c87873291f289713ac5df48b1f2010eb6963752bbd6b530416ab99fc37914a8'
test "$(awk -F '\t' '$1 == "moby-notice" { print $2 ":" $5 }' "$sources_lock")" = \
  'docker-v29.1.3@fbf3ed25f893e6ce21336f1101590e40a13934f4:b40ec5b16182103ef1cd69d42a21c98369e35bd483b3973902da4807c0755446'
test "$(awk -F '\t' '$1 == "playwright-license" { print $2 ":" $5 }' "$sources_lock")" = \
  'ae935a43d9e376e4759548f6b3c6905c7b282333:45873d00a0dd243596deb4aa23b2493b3d1f0671921bf2538ea431d7380220eb'
test "$(awk -F '\t' '$1 == "playwright-notice" { print $2 ":" $5 }' "$sources_lock")" = \
  'ae935a43d9e376e4759548f6b3c6905c7b282333:6d602191187b35b9b01d2cffa01c8469c2c8d9de8a96f1bf868e0f264f51c81d'

for package_file in "$repo_root"/container/packages/*.txt; do
  if ! sed '/^#/d; /^$/d' "$package_file" | LC_ALL=C sort -cu; then
    printf '%s must be sorted and duplicate-free\n' "$package_file" >&2
    exit 1
  fi
  if sed '/^#/d; /^$/d' "$package_file" | rg -v '^[a-z0-9][a-z0-9+.-]*$' >/dev/null; then
    printf '%s contains an invalid package name\n' "$package_file" >&2
    exit 1
  fi
done

if (( online )); then
  while IFS=$'\t' read -r component _ kind locator expected _; do
    [[ -z $component || $component == \#* || $kind == oci ]] && continue
    actual=$(curl --fail --location --silent --show-error "$locator" | sha256sum | awk '{print $1}')
    if [[ $actual != "$expected" ]]; then
      printf '%s checksum mismatch: expected %s, got %s\n' "$component" "$expected" "$actual" >&2
      exit 1
    fi
  done <"$sources_lock"
fi

printf 'container locks valid%s\n' "$([[ $online == 1 ]] && printf ' (online verified)')"
