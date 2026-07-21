#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

root=${1:-/}
output=${2:-/usr/share/doc/xenoteer/first-party-files.tsv}
policy=${3:-/usr/share/xenoteer/image-first-party-paths.tsv}
temporary=$(mktemp)
trap 'rm -f -- "$temporary"' EXIT
rm -f -- "$root$output"

classify() {
  local path=$1 pattern license evidence
  while IFS=$'\t' read -r pattern license evidence; do
    [[ -z $pattern || $pattern == \#* ]] && continue
    # Intentional policy glob; quoting would turn it into an exact match.
    # shellcheck disable=SC2053
    if [[ $path == $pattern ]]; then
      [[ -e $root$evidence ]] || {
        printf '%s maps to missing image license evidence %s\n' "$path" "$evidence" >&2
        return 2
      }
      printf '%s\t%s\n' "$license" "$evidence"
      return 0
    fi
  done <"$root$policy"
  printf 'unknown image first-party provenance: %s\n' "$path" >&2
  return 1
}

scopes=(
  /etc/s6-overlay/s6-rc.d
  /etc/xenoteer
  /usr/local/libexec/xenoteer
  /usr/share/doc/xenoteer/LICENSE
  /usr/share/doc/xenoteer/NOTICE
  /usr/share/doc/xenoteer/sources.lock
  /usr/share/xenoteer
)
[[ -e $root/usr/local/bin/xenoteerd ]] && scopes+=(/usr/local/bin/xenoteerd)

for scope in "${scopes[@]}"; do
  [[ -e $root$scope ]] || { printf 'missing first-party image scope: %s\n' "$scope" >&2; exit 1; }
  while IFS= read -r -d '' file; do
    path=${file#"$root"}
    classification=$(classify "$path")
    IFS=$'\t' read -r license evidence <<<"$classification"
    hash=$(sha256sum "$file" | awk '{print $1}')
    printf '%s\t%s\t%s\t%s\n' "$path" "$hash" "$license" "$evidence" >>"$temporary"
  done < <(
    find "$root$scope" \
      \( -type d \( -name .git -o -name .codex -o -name target -o -name dist \) -prune \) -o \
      -type f -print0
  )
done

{
  printf 'path\tsha256\tlicense_expression\tlicense_evidence\n'
  LC_ALL=C sort -u -t $'\t' -k1,1 "$temporary"
} >"$root$output"
printf 'wrote deterministic image first-party inventory: %s\n' "$output"
