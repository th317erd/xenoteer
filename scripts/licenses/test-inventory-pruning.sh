#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT

mkdir -p \
  "$fixture/source/fixtures/kept" \
  "$fixture/source/fixtures/nested/.git" \
  "$fixture/source/fixtures/nested/.codex" \
  "$fixture/source/fixtures/nested/target" \
  "$fixture/source/fixtures/nested/dist"
cp "$repo_root/LICENSE" "$fixture/source/LICENSE"
printf 'kept\n' >"$fixture/source/fixtures/kept/source.txt"
for ignored in .git target dist; do
  printf 'generated\n' >"$fixture/source/fixtures/nested/$ignored/generated.txt"
done
printf 'tracked workflow state\n' >"$fixture/source/fixtures/nested/.codex/tracked.txt"

"$repo_root/scripts/licenses/inventory-first-party.sh" \
  "$fixture/source" "$fixture/source-inventory.tsv" >/dev/null
"$repo_root/scripts/licenses/generate-source-sbom.sh" \
  "$fixture/source-inventory.tsv" "$fixture/source.spdx.json" >/dev/null

grep -Fq $'fixtures/kept/source.txt\t' "$fixture/source-inventory.tsv"
grep -Fq $'fixtures/nested/.codex/tracked.txt\t' "$fixture/source-inventory.tsv"
if grep -Eq '(^|/)(\.git|target|dist)/' "$fixture/source-inventory.tsv"; then
  printf 'source inventory included a nested build/cache directory\n' >&2
  exit 1
fi
if jq -e '[.files[].fileName | test("(^|/)(\\.git|target|dist)/")] | any' \
  "$fixture/source.spdx.json" >/dev/null; then
  printf 'source SBOM included a nested build/cache directory\n' >&2
  exit 1
fi
jq -e '[.files[].fileName] | index("fixtures/nested/.codex/tracked.txt") != null' \
  "$fixture/source.spdx.json" >/dev/null

seccomp_fixture="$fixture/source/container/spikes/browser"
mkdir -p "$seccomp_fixture/licenses/moby" "$seccomp_fixture/licenses/playwright"
printf 'profile\n' >"$seccomp_fixture/seccomp_profile.json"
printf 'license\n' >"$seccomp_fixture/licenses/moby/LICENSE"
printf 'notice\n' >"$seccomp_fixture/licenses/moby/NOTICE"
printf 'license\n' >"$seccomp_fixture/licenses/playwright/LICENSE"
printf 'notice\n' >"$seccomp_fixture/licenses/playwright/NOTICE"
"$repo_root/scripts/licenses/inventory-first-party.sh" \
  "$fixture/source" "$fixture/seccomp-inventory.tsv" >/dev/null
rm "$seccomp_fixture/licenses/playwright/NOTICE"
if "$repo_root/scripts/licenses/inventory-first-party.sh" \
  "$fixture/source" "$fixture/missing-evidence.tsv" >/dev/null 2>&1; then
  printf 'source inventory accepted a missing member of multi-path license evidence\n' >&2
  exit 1
fi

image_root="$fixture/image"
mkdir -p \
  "$image_root/etc/s6-overlay/s6-rc.d" \
  "$image_root/etc/xenoteer/kept" \
  "$image_root/usr/local/libexec/xenoteer" \
  "$image_root/usr/share/doc/xenoteer" \
  "$image_root/usr/share/novnc" \
  "$image_root/usr/share/xenoteer"
cp "$repo_root/LICENSE" "$image_root/usr/share/doc/xenoteer/LICENSE"
printf 'fixture notice\n' >"$image_root/usr/share/doc/xenoteer/NOTICE"
printf 'fixture release lock\n' >"$image_root/usr/share/doc/xenoteer/release.lock"
printf 'fixture source lock\n' >"$image_root/usr/share/doc/xenoteer/sources.lock"
printf '{}\n' >"$image_root/usr/share/novnc/mandatory.json"
printf 'kept\n' >"$image_root/etc/xenoteer/kept/runtime.txt"
mkdir -p "$image_root/etc/s6-overlay/s6-rc.d/user2"
printf 'bundle\n' >"$image_root/etc/s6-overlay/s6-rc.d/user2/type"
s6_hash=$(sha256sum "$image_root/etc/s6-overlay/s6-rc.d/user2/type" | awk '{print $1}')
{
  printf 'path\ttype\tsha256\tsymlink_target\n'
  printf '/etc/s6-overlay/s6-rc.d/user2/type\tfile\t%s\t-\n' "$s6_hash"
} >"$image_root/usr/share/doc/xenoteer/s6-overlay-files.tsv"
for ignored in .git .codex target dist; do
  mkdir -p "$image_root/etc/xenoteer/nested/$ignored"
  printf 'generated\n' >"$image_root/etc/xenoteer/nested/$ignored/generated.txt"
done
printf '%s\t%s\t%s\n' \
  '/etc/xenoteer/*' 'BUSL-1.1' '/usr/share/doc/xenoteer/LICENSE' \
  '/etc/s6-overlay/s6-rc.d/*' 'BUSL-1.1' '/usr/share/doc/xenoteer/LICENSE' \
  '/usr/share/doc/xenoteer/*' 'BUSL-1.1' '/usr/share/doc/xenoteer/LICENSE' \
  '/usr/share/novnc/mandatory.json' 'BUSL-1.1' '/usr/share/doc/xenoteer/LICENSE' \
  '/usr/share/xenoteer/*' 'BUSL-1.1' '/usr/share/doc/xenoteer/LICENSE' \
  >"$image_root/usr/share/xenoteer/test-policy.tsv"

"$repo_root/scripts/licenses/inventory-image-first-party.sh" \
  "$image_root" \
  /usr/share/doc/xenoteer/first-party-files.tsv \
  /usr/share/xenoteer/test-policy.tsv \
  /usr/share/doc/xenoteer/s6-overlay-files.tsv >/dev/null
image_inventory="$image_root/usr/share/doc/xenoteer/first-party-files.tsv"
grep -Fq $'/etc/xenoteer/kept/runtime.txt\t' "$image_inventory"
grep -Fq $'/usr/share/doc/xenoteer/release.lock\t' "$image_inventory"
grep -Fq $'/usr/share/doc/xenoteer/sources.lock\t' "$image_inventory"
if grep -Fq $'/etc/s6-overlay/s6-rc.d/user2/type\t' "$image_inventory"; then
  printf 'image first-party inventory duplicated an exact locked s6 path\n' >&2
  exit 1
fi
if grep -Eq '(^|/)(\.git|\.codex|target|dist)/' "$image_inventory"; then
  printf 'image inventory included a nested build/cache directory\n' >&2
  exit 1
fi

printf 'inventory pruning tests passed\n'
