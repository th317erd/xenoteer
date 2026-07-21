#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

if [[ $# -ne 5 ]]; then
  printf 'usage: %s SNAPSHOT SUITE RELEASE_SHA UPDATES_SHA SECURITY_SHA\n' "$0" >&2
  exit 64
fi

snapshot=$1
suite=$2
release_sha=$3
updates_sha=$4
security_sha=$5
lists=/var/lib/apt/lists

[[ $snapshot =~ ^[0-9]{8}T[0-9]{6}Z$ ]]
[[ $suite =~ ^[a-z][a-z0-9-]*$ ]]
for digest in "$release_sha" "$updates_sha" "$security_sha"; do
  [[ $digest =~ ^[a-f0-9]{64}$ ]]
done

release="$lists/snapshot.debian.org_archive_debian_${snapshot}_dists_${suite}_InRelease"
updates="$lists/snapshot.debian.org_archive_debian_${snapshot}_dists_${suite}-updates_InRelease"
security="$lists/snapshot.debian.org_archive_debian-security_${snapshot}_dists_${suite}-security_InRelease"

# Exact path checks also reject a mirror/suite substitution that happened to
# leave some other validly signed InRelease metadata in APT's list directory.
mapfile -d '' observed < <(find "$lists" -maxdepth 1 -type f -name '*_InRelease' -print0)
if ((${#observed[@]} != 3)); then
  printf 'expected exactly three Debian InRelease files, found %d\n' "${#observed[@]}" >&2
  exit 1
fi

printf '%s  %s\n%s  %s\n%s  %s\n' \
  "$release_sha" "$release" \
  "$updates_sha" "$updates" \
  "$security_sha" "$security" \
  | sha256sum --check --strict -

