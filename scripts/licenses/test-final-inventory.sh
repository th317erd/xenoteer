#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
root="$fixture/root"
mkdir -p \
  "$root/opt" \
  "$root/usr/bin" \
  "$root/usr/share/doc/demo" \
  "$root/usr/share/doc/s6-overlay" \
  "$root/usr/share/doc/xenoteer" \
  "$root/usr/share/xenoteer" \
  "$root/var/lib/dpkg/info"

printf 'Debian-owned\n' >"$root/usr/bin/demo"
printf 'demo copyright\n' >"$root/usr/share/doc/demo/copyright"
printf '%s\n' /usr/bin/demo /usr/share/doc/demo/copyright >"$root/var/lib/dpkg/info/demo.list"
printf 'first party\n' >"$root/opt/first-party"
printf 's6 binary\n' >"$root/init"
cp "$repo_root/LICENSE" "$root/usr/share/doc/xenoteer/LICENSE"
printf 'ISC evidence\n' >"$root/usr/share/doc/s6-overlay/COPYING"

package_manifest=/usr/share/doc/xenoteer/package-manifest.tsv
first_manifest=/usr/share/doc/xenoteer/first-party-files.tsv
s6_manifest=/usr/share/doc/xenoteer/s6-overlay-files.tsv
policy=/usr/share/xenoteer/final-image-exceptions.tsv
{
  printf 'binary_package\tarchitecture\tbinary_version\tsource_package\tsource_version\tcopyright_path\tcopyright_sha256\n'
  printf 'demo\tamd64\t1\tdemo\t1\t/usr/share/doc/demo/copyright\t-\n'
} >"$root$package_manifest"
printf '%s\n' \
  $'/var/lib/dpkg/info/*\tgenerated-metadata\tdpkg\tpackage-specific\t/usr/share/doc/demo/copyright\tdpkg fixture' \
  $'/usr/share/doc/s6-overlay/COPYING\tlocked-third-party-evidence\ts6-overlay-3.2.2.0\tISC\t/usr/share/doc/s6-overlay/COPYING\ts6 fixture' \
  $'/usr/share/doc/xenoteer/first-party-files.tsv\tgenerated-metadata\txenoteer\tNOASSERTION\t/usr/share/doc/xenoteer/LICENSE\tfixture' \
  $'/usr/share/doc/xenoteer/package-manifest.tsv\tgenerated-metadata\tdemo\tNOASSERTION\t/usr/share/doc/xenoteer/LICENSE\tfixture' \
  $'/usr/share/doc/xenoteer/s6-overlay-files.tsv\tgenerated-metadata\ts6-overlay-3.2.2.0\tNOASSERTION\t/usr/share/doc/xenoteer/LICENSE\tfixture' \
  >"$root$policy"

hash_first=$(sha256sum "$root/opt/first-party" | awk '{print $1}')
hash_license=$(sha256sum "$root/usr/share/doc/xenoteer/LICENSE" | awk '{print $1}')
hash_policy=$(sha256sum "$root$policy" | awk '{print $1}')
{
  printf 'path\tsha256\tlicense_expression\tlicense_evidence\n'
  printf '/opt/first-party\t%s\tBUSL-1.1\t/usr/share/doc/xenoteer/LICENSE\n' "$hash_first"
  printf '/usr/share/doc/xenoteer/LICENSE\t%s\tBUSL-1.1\t/usr/share/doc/xenoteer/LICENSE\n' "$hash_license"
  printf '/usr/share/xenoteer/final-image-exceptions.tsv\t%s\tBUSL-1.1\t/usr/share/doc/xenoteer/LICENSE\n' "$hash_policy"
} >"$root$first_manifest"
hash_s6=$(sha256sum "$root/init" | awk '{print $1}')
{
  printf 'path\ttype\tsha256\tsymlink_target\n'
  printf '/init\tfile\t%s\t-\n' "$hash_s6"
} >"$root$s6_manifest"

output=/usr/share/doc/xenoteer/final-files.tsv
"$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null
grep -Fq $'/opt/first-party\tfile\t' "$root$output"
grep -Fq $'\tfirst-party\txenoteer\tBUSL-1.1\t' "$root$output"
grep -Fq $'/init\tfile\t' "$root$output"
grep -Fq $'\tlocked-third-party\ts6-overlay-3.2.2.0\tISC\t' "$root$output"
grep -Fq $'/usr/bin/demo\tfile\t' "$root$output"
grep -Fq $'\tdebian-package\tdemo\tpackage-specific\t' "$root$output"

printf 'unknown\n' >"$root/opt/unclassified"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted an unknown copied file\n' >&2
  exit 1
fi

printf 'final-file inventory tests passed\n'
