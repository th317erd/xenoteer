#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary_directory=$(mktemp -d)
trap 'rm -rf -- "$temporary_directory"' EXIT
inputs="$temporary_directory/inputs"

{
  find "$repo_root" -name Cargo.toml -type f \
    -not -path '*/target/*' -not -path '*/.git/*' -print0
  printf '%s\0' \
    "$repo_root/Cargo.lock" \
    "$repo_root/rust-toolchain.toml"
  find "$repo_root/container/locks" "$repo_root/container/packages" \
    -maxdepth 1 -type f -print0
} | LC_ALL=C sort -zu >"$inputs"

while IFS= read -r -d '' path; do
  [[ -f $path ]] || {
    printf 'dependency-lock input is missing: %s\n' "${path#"$repo_root"/}" >&2
    exit 1
  }
  relative=${path#"$repo_root"/}
  if [[ $relative == *$'\n'* || $relative == *$'\t'* ]]; then
    printf 'dependency-lock input has an unsupported path: %q\n' "$relative" >&2
    exit 1
  fi
  printf '%s\t%s\n' "$relative" "$(sha256sum "$path" | awk '{print $1}')"
done <"$inputs" | sha256sum | awk '{print $1}'
