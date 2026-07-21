#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

root=${1:?usage: generate-novnc-manifest ROOT OUTPUT CRITICAL_HASHES}
output=${2:?usage: generate-novnc-manifest ROOT OUTPUT CRITICAL_HASHES}
critical_hashes=${3:?usage: generate-novnc-manifest ROOT OUTPUT CRITICAL_HASHES}
root=$(cd "$root" && pwd)
temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT

for required in \
  /usr/share/doc/novnc/copyright \
  /usr/share/novnc/app/ui.js \
  /usr/share/novnc/core/rfb.js \
  /usr/share/novnc/core/websock.js \
  /usr/share/novnc/package.json \
  /usr/share/novnc/vnc.html; do
  [[ -f $root$required ]] || {
    printf 'required noVNC asset is absent: %s\n' "$required" >&2
    exit 1
  }
done

# Xenoteer supplies a fail-closed viewer policy after extraction. Keeping the
# package's permissive mandatory.json out of this manifest makes that override
# an explicit first-party file instead of silently mutating a locked asset.
if [[ -e $root/usr/share/novnc/mandatory.json ]]; then
  printf 'package mandatory.json must be removed before noVNC inventory\n' >&2
  exit 1
fi

while read -r expected path; do
  [[ -z $expected || $expected == \#* ]] && continue
  [[ $expected =~ ^[a-f0-9]{64}$ && $path == /usr/share/novnc/* ]] || {
    printf 'malformed noVNC critical asset record: %s %s\n' "$expected" "$path" >&2
    exit 1
  }
  actual=$(sha256sum "$root$path" | awk '{print $1}')
  if [[ $actual != "$expected" ]]; then
    printf 'noVNC critical asset checksum mismatch: %s\n' "$path" >&2
    exit 1
  fi
done <"$critical_hashes"

rows=$temporary/rows
while IFS= read -r -d '' absolute; do
  path=${absolute#"$root"}
  if [[ $path == *$'\n'* || $path == *$'\t'* ]]; then
    printf 'unsupported control character in noVNC path: %q\n' "$path" >&2
    exit 1
  fi
  if [[ -L $absolute ]]; then
    kind='symlink'
    target=$(readlink "$absolute")
    if [[ $target == /* || /$target/ == */../* ]]; then
      printf 'unsafe noVNC symlink target at %s: %s\n' "$path" "$target" >&2
      exit 1
    fi
    hash=$(printf '%s' "$target" | sha256sum | awk '{print $1}')
  else
    kind='file'
    target=-
    hash=$(sha256sum "$absolute" | awk '{print $1}')
  fi
  printf '%s\t%s\t%s\t%s\n' "$path" "$kind" "$hash" "$target" >>"$rows"
done < <(
  find "$root/usr/share/novnc" "$root/usr/share/doc/novnc/copyright" \
    \( -type f -o -type l \) -print0
)

mkdir -p "$(dirname "$output")"
{
  printf 'path\ttype\tsha256\tsymlink_target\n'
  LC_ALL=C sort -t $'\t' -k1,1 "$rows"
} >"$output"

if [[ $(tail -n +2 "$output" | cut -f1 | LC_ALL=C sort -u | wc -l) -ne \
      $(tail -n +2 "$output" | wc -l) ]]; then
  printf 'duplicate path in noVNC extracted asset manifest\n' >&2
  exit 1
fi

printf 'wrote locked noVNC asset manifest: %s\n' "$output"
