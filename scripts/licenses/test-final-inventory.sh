#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
root="$fixture/root"
mkdir -p \
  "$root/opt" \
  "$root/etc/alternatives" \
  "$root/etc/ssl/certs" \
  "$root/usr/bin" \
  "$root/usr/lib/python3/dist-packages/demo/__pycache__" \
  "$root/usr/share/ca-certificates/mozilla" \
  "$root/usr/share/doc/ca-certificates" \
  "$root/usr/share/doc/demo" \
  "$root/usr/share/doc/dpkg" \
  "$root/usr/share/doc/s6-overlay" \
  "$root/usr/share/doc/xenoteer" \
  "$root/usr/share/doc/novnc" \
  "$root/usr/share/novnc" \
  "$root/usr/share/xenoteer" \
  "$root/var/cache/fontconfig" \
  "$root/var/lib/dpkg/alternatives" \
  "$root/var/lib/dpkg/info"

printf 'Debian-owned\n' >"$root/usr/bin/demo"
chmod 0644 "$root/usr/bin/demo"
printf 'fixture CA\n' >"$root/usr/share/ca-certificates/mozilla/Demo.crt"
printf 'CA evidence\n' >"$root/usr/share/doc/ca-certificates/copyright"
printf 'dpkg evidence\n' >"$root/usr/share/doc/dpkg/copyright"
printf 'print("demo")\n' >"$root/usr/lib/python3/dist-packages/demo/module.py"
printf 'derived bytecode\n' >"$root/usr/lib/python3/dist-packages/demo/__pycache__/module.cpython-313.pyc"
printf 'demo copyright\n' >"$root/usr/share/doc/demo/copyright"
ln -s /usr/bin/demo "$root/usr/bin/demo-link"
printf '%s\n' \
  /usr/bin/demo \
  /usr/bin/demo-link \
  /usr/lib/python3/dist-packages/demo/module.py \
  /usr/share/ca-certificates/mozilla/Demo.crt \
  /usr/share/doc/ca-certificates/copyright \
  /usr/share/doc/demo/copyright \
  /usr/share/doc/dpkg/copyright \
  >"$root/var/lib/dpkg/info/demo.list"
ln -s /etc/alternatives/demo "$root/usr/bin/demo-alt"
ln -s /usr/bin/demo "$root/etc/alternatives/demo"
ln -s /usr/share/man/man1/demo.1.gz "$root/etc/alternatives/demo.1.gz"
printf '%s\n' demo demo.1.gz /usr/share/man/man1/demo.1.gz /usr/bin/demo \
  >"$root/var/lib/dpkg/alternatives/demo"
ln -s /usr/share/man/man7/demo-builtins.7.gz \
  "$root/etc/alternatives/demo-builtins.7.gz"
printf '%s\n' auto /usr/share/man/man7/demo-builtins.7.gz 10 \
  >"$root/var/lib/dpkg/alternatives/demo-builtins.7.gz"
ln -s /usr/share/ca-certificates/mozilla/Demo.crt "$root/etc/ssl/certs/Demo.pem"
ln -s Demo.pem "$root/etc/ssl/certs/01234567.0"

package_manifest=/usr/share/doc/xenoteer/package-manifest.tsv
debian_installed_manifest=/usr/share/doc/xenoteer/debian-installed-files.tsv
{
  printf 'binary_package\tarchitecture\tbinary_version\tsource_package\tsource_version\tcopyright_path\tcopyright_sha256\tdeb_repository_path\tdeb_sha256\tdirect_request\n'
  printf 'demo\tamd64\t1\tdemo\t1\t/usr/share/doc/demo/copyright\t%s\tpool/main/d/demo/demo_1_amd64.deb\t%s\ttrue\n' \
    "$(sha256sum "$root/usr/share/doc/demo/copyright" | awk '{print $1}')" \
    '0000000000000000000000000000000000000000000000000000000000000000'
} >"$root$package_manifest"
{
  printf '%s  %s\n' \
    "$(md5sum "$root/usr/bin/demo" | awk '{print $1}')" usr/bin/demo
  printf '%s  %s\n' \
    "$(md5sum "$root/usr/lib/python3/dist-packages/demo/module.py" | awk '{print $1}')" \
    usr/lib/python3/dist-packages/demo/module.py
  printf '%s  %s\n' \
    "$(md5sum "$root/usr/share/ca-certificates/mozilla/Demo.crt" | awk '{print $1}')" \
    usr/share/ca-certificates/mozilla/Demo.crt
  printf '%s  %s\n' \
    "$(md5sum "$root/usr/share/doc/ca-certificates/copyright" | awk '{print $1}')" \
    usr/share/doc/ca-certificates/copyright
  printf '%s  %s\n' \
    "$(md5sum "$root/usr/share/doc/demo/copyright" | awk '{print $1}')" \
    usr/share/doc/demo/copyright
  printf '%s  %s\n' \
    "$(md5sum "$root/usr/share/doc/dpkg/copyright" | awk '{print $1}')" \
    usr/share/doc/dpkg/copyright
} >"$root/var/lib/dpkg/info/demo.md5sums"

{
  printf 'Package: demo\nStatus: install ok installed\nArchitecture: amd64\nVersion: 1\nMaintainer: Fixture <fixture@example.invalid>\nDescription: fixture\n\n'
  printf 'Package: untracked\nStatus: install ok installed\nArchitecture: amd64\nVersion: 1\nMaintainer: Fixture <fixture@example.invalid>\nDescription: hostile fixture\n'
} >"$root/var/lib/dpkg/status"
if "$repo_root/scripts/licenses/generate-debian-installed-manifest.sh" \
  "$root" "$debian_installed_manifest" "$package_manifest" >/dev/null 2>&1; then
  printf 'installed-baseline generator accepted a package absent from signed provenance\n' >&2
  exit 1
fi
printf 'Package: demo\nStatus: install ok installed\nArchitecture: amd64\nVersion: 1\nMaintainer: Fixture <fixture@example.invalid>\nDescription: fixture\n' \
  >"$root/var/lib/dpkg/status"

"$repo_root/scripts/licenses/generate-debian-installed-manifest.sh" \
  "$root" "$debian_installed_manifest" "$package_manifest" >/dev/null

printf 'first party\n' >"$root/opt/first-party"
printf 's6 binary\n' >"$root/init"
cp "$repo_root/LICENSE" "$root/usr/share/doc/xenoteer/LICENSE"
printf 'ISC evidence\n' >"$root/usr/share/doc/s6-overlay/COPYING"
printf 'noVNC asset\n' >"$root/usr/share/novnc/vnc.html"
printf 'noVNC copyright\n' >"$root/usr/share/doc/novnc/copyright"

first_manifest=/usr/share/doc/xenoteer/first-party-files.tsv
s6_manifest=/usr/share/doc/xenoteer/s6-overlay-files.tsv
policy=/usr/share/xenoteer/final-image-exceptions.tsv
novnc_manifest=/usr/share/doc/xenoteer/novnc-files.tsv
printf '%s\n' \
  $'/usr/share/doc/s6-overlay/COPYING\tlocked-third-party-evidence\ts6-overlay-3.2.2.0\tISC\t/usr/share/doc/s6-overlay/COPYING\ts6 fixture' \
  $'/usr/share/doc/xenoteer/first-party-files.tsv\tgenerated-metadata\txenoteer\tNOASSERTION\t/usr/share/doc/xenoteer/LICENSE\tfixture' \
  $'/usr/share/doc/xenoteer/debian-installed-files.tsv\tgenerated-metadata\tdebian-installed-baseline\tNOASSERTION\t/usr/share/doc/xenoteer/LICENSE\tfixture' \
  $'/usr/share/doc/xenoteer/package-manifest.tsv\tgenerated-metadata\tdemo\tNOASSERTION\t/usr/share/doc/xenoteer/LICENSE\tfixture' \
  $'/usr/share/doc/xenoteer/s6-overlay-files.tsv\tgenerated-metadata\ts6-overlay-3.2.2.0\tNOASSERTION\t/usr/share/doc/xenoteer/LICENSE\tfixture' \
  $'/usr/share/doc/xenoteer/novnc-files.tsv\tgenerated-metadata\tnovnc-1:1.6.0-2\tNOASSERTION\t/usr/share/doc/xenoteer/LICENSE\tfixture' \
  $'/var/lib/dpkg/status\tgenerated-metadata\tdpkg\tpackage-specific\t/usr/share/doc/xenoteer/LICENSE\tfixture dpkg database' \
  $'/opt/generated-link-exact\tgenerated-symlink\tdpkg\tpackage-specific\t/usr/share/doc/xenoteer/LICENSE\tfixture exact-kind rejection' \
  $'/opt/generated-config-exact\tgenerated-config\tdpkg\tpackage-specific\t/usr/share/doc/xenoteer/LICENSE\tfixture regular-kind rejection' \
  $'/opt/generated-links/*\tgenerated-symlink\tdpkg\tpackage-specific\t/usr/share/doc/xenoteer/LICENSE\tfixture broad-glob rejection' \
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
novnc_hash=$(sha256sum "$root/usr/share/novnc/vnc.html" | awk '{print $1}')
novnc_copyright_hash=$(sha256sum "$root/usr/share/doc/novnc/copyright" | awk '{print $1}')
{
  printf 'path\ttype\tsha256\tsymlink_target\n'
  printf '/usr/share/doc/novnc/copyright\tfile\t%s\t-\n' "$novnc_copyright_hash"
  printf '/usr/share/novnc/vnc.html\tfile\t%s\t-\n' "$novnc_hash"
} >"$root$novnc_manifest"

output=/usr/share/doc/xenoteer/final-files.tsv
"$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null
grep -Fq $'/opt/first-party\tfile\t' "$root$output"
grep -Fq $'\tfirst-party\txenoteer\tBUSL-1.1\t' "$root$output"
grep -Fq $'/init\tfile\t' "$root$output"
grep -Fq $'\tlocked-third-party\ts6-overlay-3.2.2.0\tISC\t' "$root$output"
grep -Fq $'/usr/bin/demo\tfile\t' "$root$output"
grep -Fq $'\tdebian-package\tdemo\tpackage-specific\t' "$root$output"
grep -Fq $'/usr/lib/python3/dist-packages/demo/__pycache__/module.cpython-313.pyc\tfile\t' "$root$output"
grep -Fq $'\tgenerated-cache\tdemo\tpackage-specific\t' "$root$output"
grep -Fq $'/usr/bin/demo-alt\tsymlink\t' "$root$output"
grep -Fq $'\tgenerated-symlink\tdpkg\tpackage-specific\t' "$root$output"
grep -Fq $'/etc/alternatives/demo.1.gz\tsymlink\t' "$root$output"
grep -Fq $'/etc/alternatives/demo-builtins.7.gz\tsymlink\t' "$root$output"
[[ $(grep -Fc $'update-alternatives verified dangling link' "$root$output") -eq 2 ]]
grep -Fq $'/etc/ssl/certs/Demo.pem\tsymlink\t' "$root$output"
grep -Fq $'/etc/ssl/certs/01234567.0\tsymlink\t' "$root$output"
grep -Fq $'\tgenerated-symlink\tca-certificates\tpackage-specific\t' "$root$output"
grep -Fq $'/usr/share/novnc/vnc.html\tfile\t' "$root$output"
grep -Fq $'\tlocked-third-party\tnovnc-1:1.6.0-2\tpackage-specific\t' "$root$output"
grep -Fq $'/usr/share/doc/xenoteer/final-files.tsv\tfile\tSELF-REFERENTIAL\t-\t644\t' "$root$output"
grep -Fq $'/usr/bin/demo\tfile\t' "$root$debian_installed_manifest"
grep -Fq $'\tdemo\tdpkg-md5' "$root$debian_installed_manifest"

printf 'overwritten package executable\n' >"$root/usr/bin/demo"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted overwritten Debian payload content\n' >&2
  exit 1
fi
printf 'Debian-owned\n' >"$root/usr/bin/demo"

chmod 0755 "$root/usr/bin/demo"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted changed Debian payload mode\n' >&2
  exit 1
fi
chmod 0644 "$root/usr/bin/demo"

ln -sfn /bin/false "$root/usr/bin/demo-link"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted changed Debian symlink target\n' >&2
  exit 1
fi
ln -sfn /usr/bin/demo "$root/usr/bin/demo-link"

rm "$root/opt/first-party"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted a missing first-party manifest entry\n' >&2
  exit 1
fi
printf 'first party\n' >"$root/opt/first-party"

rm "$root/init"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted a missing s6 manifest entry\n' >&2
  exit 1
fi
printf 's6 binary\n' >"$root/init"

printf 'not a symlink\n' >"$root/opt/generated-link-exact"
set +e
"$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1
kind_status=$?
set -e
if (( kind_status != 2 )); then
  printf 'generated-symlink wrong-kind status was %s, expected 2\n' "$kind_status" >&2
  exit 1
fi
rm "$root/opt/generated-link-exact"

ln -s /usr/bin/demo "$root/opt/generated-config-exact"
set +e
"$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1
kind_status=$?
set -e
if (( kind_status != 2 )); then
  printf 'generated-config exception accepted a symlink\n' >&2
  exit 1
fi
rm "$root/opt/generated-config-exact"

mkdir -p "$root/opt/generated-links"
ln -s /usr/bin/demo "$root/opt/generated-links/unreviewed"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'broad exception accepted a path absent from the installed baseline\n' >&2
  exit 1
fi
rm "$root/opt/generated-links/unreviewed"
rmdir "$root/opt/generated-links"

printf 'tampered noVNC asset\n' >"$root/usr/share/novnc/vnc.html"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted a noVNC asset that differed from its lock\n' >&2
  exit 1
fi
printf 'noVNC asset\n' >"$root/usr/share/novnc/vnc.html"

rm "$root/usr/share/novnc/vnc.html"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted a missing locked noVNC asset\n' >&2
  exit 1
fi
printf 'noVNC asset\n' >"$root/usr/share/novnc/vnc.html"

printf 'unknown\n' >"$root/opt/unclassified"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted an unknown copied file\n' >&2
  exit 1
fi
rm "$root/opt/unclassified"

printf 'hidden payload\n' >"$root/var/lib/dpkg/info/demo.payload"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted an unknown dpkg-info suffix\n' >&2
  exit 1
fi
rm "$root/var/lib/dpkg/info/demo.payload"

printf 'hidden payload\n' >"$root/var/cache/fontconfig/not-a-cache"
if "$repo_root/scripts/licenses/inventory-final-image.sh" \
  "$root" "$output" "$first_manifest" "$s6_manifest" "$policy" >/dev/null 2>&1; then
  printf 'final-file inventory accepted an unknown fontconfig cache filename\n' >&2
  exit 1
fi

printf 'final-file inventory tests passed\n'
