#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

root=${1:-/}
output=${2:-/usr/share/doc/xenoteer/debian-installed-files.tsv}
package_manifest=${3:-/usr/share/doc/xenoteer/package-manifest.tsv}
root=$(cd "$root" && pwd)
temporary_directory=$(mktemp -d)
trap 'rm -rf -- "$temporary_directory"' EXIT

inside_root() {
  if [[ $root == / ]]; then
    printf '%s' "$1"
  else
    printf '%s%s' "$root" "$1"
  fi
}

output_path=$(inside_root "$output")
package_manifest_path=$(inside_root "$package_manifest")
dpkg_info=$(inside_root /var/lib/dpkg/info)
dpkg_admin=$(inside_root /var/lib/dpkg)
[[ -f $package_manifest_path ]] || {
  printf 'Debian package provenance manifest is missing: %s\n' "$package_manifest" >&2
  exit 1
}
[[ -d $dpkg_info ]] || { printf 'dpkg info directory is missing\n' >&2; exit 1; }
rm -f -- "$output_path"

declare -A package_present dpkg_owner md5_expected

while IFS=$'\t' read -r package _ _ _ _ _ _ _ deb_sha _; do
  [[ $package == binary_package ]] && continue
  [[ -n $package && $deb_sha =~ ^[a-f0-9]{64}$ ]] || {
    printf 'invalid package provenance row while generating installed baseline: %s\n' "$package" >&2
    exit 1
  }
  [[ -z ${package_present[$package]:-} ]] || {
    printf 'duplicate package in provenance manifest: %s\n' "$package" >&2
    exit 1
  }
  package_present["$package"]=true
  package_present["${package%%:*}"]=true
done <"$package_manifest_path"

# Treat dpkg's installed-status database as the completeness authority. Package
# file lists normally cover every installed binary package, but provenance must
# not depend on that implementation detail (an installed package may have an
# empty or otherwise unusual payload).
if [[ -f $dpkg_admin/status ]]; then
  installed_package_count=0
  while IFS=$'\t' read -r package status; do
    [[ $status == installed ]] || continue
    ((installed_package_count += 1))
    [[ -n ${package_present[$package]:-} \
      || -n ${package_present[${package%%:*}]:-} ]] || {
      printf 'installed package has no signed package provenance row: %s\n' \
        "$package" >&2
      exit 1
    }
  done < <(
    dpkg-query --admindir="$dpkg_admin" -W \
      -f='${binary:Package}\t${db:Status-Status}\n'
  )
  ((installed_package_count > 0)) || {
    printf 'dpkg status database contains no installed packages\n' >&2
    exit 1
  }
fi

record_owner() {
  local path=$1 package=$2
  [[ $path == /* ]] || return 0
  if [[ -n ${dpkg_owner[$path]:-} && ,${dpkg_owner[$path]}, != *,$package,* ]]; then
    dpkg_owner["$path"]+=",$package"
  elif [[ -z ${dpkg_owner[$path]:-} ]]; then
    dpkg_owner["$path"]=$package
  fi
}

while IFS= read -r -d '' list; do
  package=${list##*/}
  package=${package%.list}
  [[ -n ${package_present[$package]:-} ]] || {
    printf 'dpkg file list has no signed package provenance row: %s\n' "$package" >&2
    exit 1
  }
  while IFS= read -r path || [[ -n $path ]]; do
    record_owner "$path" "$package"
  done <"$list"
done < <(find "$dpkg_info" -maxdepth 1 -type f -name '*.list' -print0)

# Conffiles are not consistently present in package *.list files. Snapshot the
# active dpkg conffile database as part of the same all-installed-package map.
if [[ -f $dpkg_admin/status ]]; then
  while IFS=$'\t' read -r path package; do
    [[ -n ${package_present[$package]:-} ]] || {
      printf 'dpkg conffile has no signed package provenance row: %s (%s)\n' \
        "$path" "$package" >&2
      exit 1
    }
    record_owner "$path" "$package"
  done < <(
    dpkg-query --admindir="$dpkg_admin" -W \
      -f='Package=${binary:Package}\n${Conffiles}\n' \
      | awk '
          /^Package=/ { package = substr($0, 9); next }
          /^ \/[^ ]+ / { print $1 "\t" package }
        '
  )
fi

# Verify every still-present regular payload file for which dpkg supplies an
# upstream md5. Missing files can be intentional Debian slim pruning, but a
# present mismatched payload is never admitted into the trusted baseline.
while IFS= read -r -d '' sums; do
  while IFS= read -r line || [[ -n $line ]]; do
    [[ $line =~ ^([a-f0-9]{32})[[:space:]][[:space:]](.+)$ ]] || {
      printf 'malformed dpkg md5sums row in %s\n' "$sums" >&2
      exit 1
    }
    expected=${BASH_REMATCH[1]}
    path=/${BASH_REMATCH[2]}
    if [[ -n ${md5_expected[$path]:-} && ${md5_expected[$path]} != "$expected" ]]; then
      printf 'conflicting dpkg payload md5 for %s\n' "$path" >&2
      exit 1
    fi
    md5_expected["$path"]=$expected
  done <"$sums"
done < <(find "$dpkg_info" -maxdepth 1 -type f -name '*.md5sums' -print0)

rows=$temporary_directory/rows
while IFS= read -r -d '' absolute; do
  if [[ $root == / ]]; then path=$absolute; else path=${absolute#"$root"}; fi
  case "$path" in
    "$output"|/.dockerenv|/etc/hostname|/etc/hosts|/etc/resolv.conf|/tmp/*|/var/lib/apt/lists/*|/usr/local/bin/verify-apt-metadata)
      continue
      ;;
  esac
  if [[ $path == *$'\n'* || $path == *$'\t'* ]]; then
    printf 'unsupported control character in installed baseline path: %q\n' "$path" >&2
    exit 1
  fi

  if [[ -L $absolute ]]; then
    kind='symlink'
    target=$(readlink "$absolute")
    hash=$(printf '%s' "$target" | sha256sum | awk '{print $1}')
  else
    kind='file'
    target=-
    hash=$(sha256sum "$absolute" | awk '{print $1}')
  fi
  read -r mode uid gid < <(stat -c '%a %u %g' -- "$absolute")
  owner=${dpkg_owner[$path]:--}
  verification=post-install-state
  if [[ $owner != - ]]; then
    IFS=',' read -r -a owners <<<"$owner"
    for package in "${owners[@]}"; do
      [[ -n ${package_present[$package]:-} ]] || {
        printf 'installed path owner lacks signed package provenance: %s (%s)\n' \
          "$path" "$package" >&2
        exit 1
      }
    done
    verification=installed-package-state
    if [[ -n ${md5_expected[$path]:-} ]]; then
      [[ $kind == file ]] || {
        printf 'dpkg md5 payload unexpectedly became a symlink: %s\n' "$path" >&2
        exit 1
      }
      actual_md5=$(md5sum "$absolute" | awk '{print $1}')
      [[ $actual_md5 == "${md5_expected[$path]}" ]] || {
        printf 'installed Debian payload differs from dpkg md5: %s\n' "$path" >&2
        exit 1
      }
      verification=dpkg-md5
    fi
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$path" "$kind" "$hash" "$target" "$mode" "$uid" "$gid" \
    "$owner" "$verification" >>"$rows"
done < <(
  find "$root" -xdev \
    \( -path "$temporary_directory" -prune \) -o \
    \( -type f -o -type l \) -print0
)

mkdir -p "$(dirname "$output_path")"
{
  printf 'path\ttype\tsha256\tsymlink_target\tmode\tuid\tgid\tdpkg_owner\tverification\n'
  LC_ALL=C sort -t $'\t' -k1,1 "$rows"
} >"$output_path"
printf 'wrote exact post-install Debian filesystem baseline: %s\n' "$output"
