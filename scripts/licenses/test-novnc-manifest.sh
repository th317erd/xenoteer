#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
root=$fixture/root
critical=$fixture/critical.sha256
output=$fixture/manifest.tsv

mkdir -p \
  "$root/usr/share/doc/novnc" \
  "$root/usr/share/novnc/app" \
  "$root/usr/share/novnc/core"
printf 'copyright\n' >"$root/usr/share/doc/novnc/copyright"
printf 'ui\n' >"$root/usr/share/novnc/app/ui.js"
printf 'rfb\n' >"$root/usr/share/novnc/core/rfb.js"
printf 'websock\n' >"$root/usr/share/novnc/core/websock.js"
printf 'package\n' >"$root/usr/share/novnc/package.json"
printf 'vnc\n' >"$root/usr/share/novnc/vnc.html"
ln -s vnc.html "$root/usr/share/novnc/vnc_auto.html"

for path in \
  /usr/share/novnc/app/ui.js \
  /usr/share/novnc/core/rfb.js \
  /usr/share/novnc/core/websock.js \
  /usr/share/novnc/package.json \
  /usr/share/novnc/vnc.html; do
  printf '%s  %s\n' "$(sha256sum "$root$path" | awk '{print $1}')" "$path" >>"$critical"
done

"$repo_root/scripts/licenses/generate-novnc-manifest.sh" \
  "$root" "$output" "$critical" >/dev/null
tail -n +2 "$output" | LC_ALL=C sort -c -t $'\t' -k1,1
grep -Fq $'/usr/share/novnc/vnc_auto.html\tsymlink\t' "$output"
grep -Fq $'\tvnc.html' "$output"

printf '{}\n' >"$root/usr/share/novnc/mandatory.json"
if "$repo_root/scripts/licenses/generate-novnc-manifest.sh" \
  "$root" "$output" "$critical" >/dev/null 2>&1; then
  printf 'noVNC manifest accepted the package mandatory.json\n' >&2
  exit 1
fi
rm "$root/usr/share/novnc/mandatory.json"

printf 'tampered\n' >"$root/usr/share/novnc/app/ui.js"
if "$repo_root/scripts/licenses/generate-novnc-manifest.sh" \
  "$root" "$output" "$critical" >/dev/null 2>&1; then
  printf 'noVNC manifest accepted a critical-asset checksum mismatch\n' >&2
  exit 1
fi

printf 'noVNC manifest tests passed\n'
