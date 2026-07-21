#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
baseline="$repo_root/container/spikes/browser/docker-default-seccomp.json"
profile="$repo_root/container/spikes/browser/seccomp_profile.json"
temporary_directory=$(mktemp -d)
trap 'rm -rf -- "$temporary_directory"' EXIT

printf '%s  %s\n' \
  f17cb7cf3c40ab6a42d978a3eea027062f18ee72d2ba5edc3a5cbdf58c67ab58 \
  "$baseline" | sha256sum --check --strict - >/dev/null
printf '%s  %s\n' \
  40166786956047cc615ab75b23730545917e8862815d09cd816971ead747902c \
  "$profile" | sha256sum --check --strict - >/dev/null
printf '%s  %s\n' \
  7c87873291f289713ac5df48b1f2010eb6963752bbd6b530416ab99fc37914a8 \
  "$repo_root/container/spikes/browser/licenses/moby/LICENSE" \
  b40ec5b16182103ef1cd69d42a21c98369e35bd483b3973902da4807c0755446 \
  "$repo_root/container/spikes/browser/licenses/moby/NOTICE" \
  45873d00a0dd243596deb4aa23b2493b3d1f0671921bf2538ea431d7380220eb \
  "$repo_root/container/spikes/browser/licenses/playwright/LICENSE" \
  6d602191187b35b9b01d2cffa01c8469c2c8d9de8a96f1bf868e0f264f51c81d \
  "$repo_root/container/spikes/browser/licenses/playwright/NOTICE" \
  | sha256sum --check --strict - >/dev/null

jq -e '
  .defaultAction == "SCMP_ACT_ERRNO"
  and (.syscalls[0].comment == "Allow create user namespaces")
  and (.syscalls[0].action == "SCMP_ACT_ALLOW")
  and (.syscalls[0].args == [])
  and (.syscalls[0].includes == {})
  and (.syscalls[0].excludes == {})
  and ((.syscalls[0].names | sort) == ["clone", "setns", "unshare"])
' "$profile" >/dev/null

jq 'del(.syscalls[0])' "$profile" >"$temporary_directory/stripped.json"
jq -S . "$baseline" >"$temporary_directory/baseline.sorted.json"
jq -S . "$temporary_directory/stripped.json" >"$temporary_directory/stripped.sorted.json"
cmp "$temporary_directory/baseline.sorted.json" "$temporary_directory/stripped.sorted.json"

# These high-risk calls must not enter through a browser-specific unconditional
# allow. The semantic baseline comparison above also prevents any hidden delta.
if jq -e '
  .syscalls[0].names
  | any(. == "bpf" or . == "keyctl" or . == "mount" or . == "ptrace"
        or . == "request_key" or . == "socket" or . == "socketcall")
' "$profile" >/dev/null; then
  printf 'browser user-namespace extension contains a forbidden syscall\n' >&2
  exit 1
fi

printf 'browser seccomp profile is a pinned Docker baseline plus clone/setns/unshare only\n'
