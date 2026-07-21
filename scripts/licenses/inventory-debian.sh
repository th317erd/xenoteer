#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

root=${1:-/}
output=${2:-/usr/share/doc/xenoteer/package-manifest.tsv}
admindir="$root/var/lib/dpkg"
temporary=$(mktemp)
trap 'rm -f -- "$temporary"' EXIT

if [[ ! -d $admindir ]]; then
  printf 'dpkg database not found beneath %s\n' "$root" >&2
  exit 1
fi

dpkg-query --admindir="$admindir" -W \
  -f='${binary:Package}\t${Architecture}\t${Version}\t${source:Package}\t${source:Version}\n' \
  | LC_ALL=C sort >"$temporary"

mkdir -p "$(dirname "$output")"
printf 'binary_package\tarchitecture\tbinary_version\tsource_package\tsource_version\tcopyright_path\tcopyright_sha256\n' >"$output"

while IFS=$'\t' read -r package architecture version source source_version; do
  package_doc=${package%%:*}
  evidence="/usr/share/doc/$package_doc/copyright"
  if [[ -z $source || -z $source_version ]]; then
    printf 'unknown source provenance for Debian package %s\n' "$package" >&2
    exit 1
  fi
  if [[ ! -e $root$evidence ]]; then
    printf 'missing copyright evidence for Debian package %s at %s\n' "$package" "$evidence" >&2
    exit 1
  fi
  evidence_hash=$(sha256sum "$root$evidence" | awk '{print $1}')
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$package" "$architecture" "$version" "$source" "$source_version" "$evidence" "$evidence_hash" >>"$output"
done <"$temporary"

printf 'wrote deterministic Debian package inventory: %s\n' "$output"
