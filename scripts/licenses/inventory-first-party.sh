#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
scan_root=${1:-$repo_root}
output=${2:-$repo_root/dist/licenses/first-party-files.tsv}
policy="$repo_root/container/licenses/first-party-paths.tsv"
scan_root=$(cd "$scan_root" && pwd)
mkdir -p "$(dirname "$output")"
temporary=$(mktemp)
file_list=$(mktemp)
trap 'rm -f -- "$temporary" "$file_list"' EXIT

classify() {
  local path=$1 pattern license evidence evidence_path
  while IFS=$'\t' read -r pattern license evidence; do
    [[ -z $pattern || $pattern == \#* ]] && continue
    # Intentional policy glob; quoting would turn it into an exact match.
    # shellcheck disable=SC2053
    if [[ $path == $pattern ]]; then
      while IFS= read -r evidence_path || [[ -n $evidence_path ]]; do
        if [[ -z $evidence_path || ! -f $scan_root/$evidence_path ]]; then
          printf '%s maps to missing license evidence\n' "$path" >&2
          return 2
        fi
      done < <(printf '%s' "$evidence" | tr '|' '\n')
      printf '%s\t%s\n' "$license" "$evidence"
      return 0
    fi
  done <"$policy"
  printf 'unknown first-party provenance: %s\n' "$path" >&2
  return 1
}

find "$scan_root" \
  \( -type d \( -name .git -o -name target -o -name dist \) -prune \) -o \
  -type f -print0 >"$file_list"

while IFS= read -r -d '' absolute; do
  path=${absolute#"$scan_root"/}
  if [[ $path == *$'\n'* || $path == *$'\t'* ]]; then
    printf 'unsupported control character in source path: %q\n' "$path" >&2
    exit 1
  fi
  classification=$(classify "$path")
  IFS=$'\t' read -r license evidence <<<"$classification"
  file_hash=$(sha256sum "$absolute" | awk '{print $1}')
  path_hash=$(printf '%s' "$path" | sha256sum | awk '{print $1}')
  printf '%s\t%s\t%s\t%s\t%s\n' "$path" "$file_hash" "$license" "$evidence" "$path_hash" >>"$temporary"
done <"$file_list"

{
  printf 'path\tsha256\tlicense_expression\tlicense_evidence\tpath_sha256\n'
  LC_ALL=C sort -t $'\t' -k1,1 "$temporary"
} >"$output"

printf 'wrote deterministic first-party inventory: %s\n' "$output"
