#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

root=${1:?extracted s6 root is required}
output=${2:?output manifest is required}
root=$(cd "$root" && pwd)
temporary=$(mktemp)
trap 'rm -f -- "$temporary"' EXIT

while IFS= read -r -d '' absolute; do
  if [[ $root == / ]]; then
    path=$absolute
  else
    path=${absolute#"$root"}
  fi
  if [[ $path == *$'\n'* || $path == *$'\t'* ]]; then
    printf 'unsupported control character in s6 path: %q\n' "$path" >&2
    exit 1
  fi
  if [[ -L $absolute ]]; then
    kind='symlink'
    target=$(readlink "$absolute")
    hash=$(printf '%s' "$target" | sha256sum | awk '{print $1}')
    detail=$target
  else
    kind='file'
    hash=$(sha256sum "$absolute" | awk '{print $1}')
    detail=-
  fi
  printf '%s\t%s\t%s\t%s\n' "$path" "$kind" "$hash" "$detail" >>"$temporary"
done < <(find "$root" \( -type f -o -type l \) -print0)

{
  printf 'path\ttype\tsha256\tsymlink_target\n'
  LC_ALL=C sort -t $'\t' -k1,1 "$temporary"
} >"$output"
printf 'wrote locked s6 extracted-file manifest: %s\n' "$output"
