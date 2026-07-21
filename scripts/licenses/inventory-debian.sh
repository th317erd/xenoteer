#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

root=${1:-/}
output=${2:-/usr/share/doc/xenoteer/package-manifest.tsv}
requested_packages=${3:-}
admindir="$root/var/lib/dpkg"
temporary=$(mktemp)
requested=$(mktemp)
apt_specs=$(mktemp)
apt_records=$(mktemp)
apt_rows=$(mktemp)
trap 'rm -f -- "$temporary" "$requested" "$apt_specs" "$apt_records" "$apt_rows"' EXIT

if [[ ! -d $admindir ]]; then
  printf 'dpkg database not found beneath %s\n' "$root" >&2
  exit 1
fi

dpkg-query --admindir="$admindir" -W \
  -f='${binary:Package}\t${Architecture}\t${Version}\t${source:Package}\t${source:Version}\n' \
  | LC_ALL=C sort >"$temporary"

if [[ -n $requested_packages ]]; then
  sed '/^#/d; /^$/d' "$requested_packages" | LC_ALL=C sort -u >"$requested"
fi

awk -F '\t' '{ package=$1; sub(/:.*/, "", package); print package ":" $2 "=" $3 }' \
  "$temporary" >"$apt_specs"
xargs -r apt-cache show --no-all-versions <"$apt_specs" >"$apt_records"
awk '
  BEGIN { RS=""; FS="\n"; OFS="\t" }
  {
    package=architecture=version=filename=sha256=""
    for (line=1; line<=NF; line++) {
      if ($line ~ /^Package: /) package=substr($line, 10)
      else if ($line ~ /^Architecture: /) architecture=substr($line, 15)
      else if ($line ~ /^Version: /) version=substr($line, 10)
      else if ($line ~ /^Filename: /) filename=substr($line, 11)
      else if ($line ~ /^SHA256: /) sha256=substr($line, 9)
    }
    if (package != "" && architecture != "" && version != "")
      print package, architecture, version, filename, sha256
  }
' "$apt_records" | LC_ALL=C sort -u >"$apt_rows"

declare -A repository_by_key sha256_by_key
while IFS=$'\t' read -r package architecture version repository_path deb_sha256; do
  key="$package"$'\t'"$architecture"$'\t'"$version"
  if [[ -n ${repository_by_key[$key]:-} ]]; then
    printf 'duplicate snapshot archive record for %s %s %s\n' \
      "$package" "$architecture" "$version" >&2
    exit 1
  fi
  repository_by_key["$key"]=$repository_path
  sha256_by_key["$key"]=$deb_sha256
done <"$apt_rows"

mkdir -p "$(dirname "$output")"
printf 'binary_package\tarchitecture\tbinary_version\tsource_package\tsource_version\tcopyright_path\tcopyright_sha256\tdeb_repository_path\tdeb_sha256\tdirect_request\n' >"$output"

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
  key="${package%%:*}"$'\t'"$architecture"$'\t'"$version"
  repository_path=${repository_by_key[$key]:-}
  deb_sha256=${sha256_by_key[$key]:-}
  if [[ ! $repository_path =~ ^pool/ || \
        ! $deb_sha256 =~ ^[a-f0-9]{64}$ ]]; then
    printf 'snapshot archive provenance mismatch for Debian package %s %s %s\n' \
      "$package" "$architecture" "$version" >&2
    exit 1
  fi
  if grep -Fqx "${package%%:*}" "$requested"; then direct=true; else direct=false; fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$package" "$architecture" "$version" "$source" "$source_version" \
    "$evidence" "$evidence_hash" "$repository_path" "$deb_sha256" "$direct" >>"$output"
done <"$temporary"

printf 'wrote deterministic Debian package inventory: %s\n' "$output"
