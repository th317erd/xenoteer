#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

root=${1:-/}
output=${2:-/usr/share/doc/xenoteer/final-files.tsv}
first_party=${3:-/usr/share/doc/xenoteer/first-party-files.tsv}
s6_manifest=${4:-/usr/share/doc/xenoteer/s6-overlay-files.tsv}
exceptions=${5:-/usr/share/xenoteer/final-image-exceptions.tsv}
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
first_party_path=$(inside_root "$first_party")
s6_manifest_path=$(inside_root "$s6_manifest")
exceptions_path=$(inside_root "$exceptions")
package_manifest_path=$(inside_root /usr/share/doc/xenoteer/package-manifest.tsv)
[[ -f $first_party_path ]] || { printf 'first-party image manifest is missing\n' >&2; exit 1; }
[[ -f $s6_manifest_path ]] || { printf 's6 image manifest is missing\n' >&2; exit 1; }
[[ -f $exceptions_path ]] || { printf 'final-image exception policy is missing\n' >&2; exit 1; }
[[ -f $package_manifest_path ]] || { printf 'Debian package manifest is missing\n' >&2; exit 1; }
rm -f -- "$output_path"

declare -A dpkg_owner debian_evidence first_hash first_license first_evidence
declare -A s6_type s6_hash s6_target

while IFS= read -r -d '' list; do
  package=${list##*/}
  package=${package%.list}
  while IFS= read -r path || [[ -n $path ]]; do
    [[ $path == /* ]] || continue
    if [[ -n ${dpkg_owner[$path]:-} && ${dpkg_owner[$path]} != "$package" ]]; then
      # Comma is outside Debian's binary package-name alphabet. A plus is not:
      # libstdc++6 is a common counterexample.
      dpkg_owner["$path"]+=",$package"
    else
      dpkg_owner["$path"]=$package
    fi
  done <"$list"
done < <(find "$(inside_root /var/lib/dpkg/info)" -maxdepth 1 -type f -name '*.list' -print0)

# dpkg intentionally omits conffiles from package *.list files. Add the active
# conffile database so files such as /etc/profile and PAM's generated baseline
# remain attributed to the binary package that installed them.
while IFS=$'\t' read -r path package; do
  [[ $path == /* && -n $package ]] || continue
  dpkg_owner["$path"]=$package
done < <(
  dpkg-query --admindir="$(inside_root /var/lib/dpkg)" -W \
    -f='Package=${binary:Package}\n${Conffiles}\n' \
    | awk '
        /^Package=/ { package = substr($0, 9); next }
        /^ \/[^ ]+ / { print $1 "\t" package }
      '
)

while IFS=$'\t' read -r package _ _ _ _ evidence _; do
  [[ $package == binary_package ]] && continue
  debian_evidence["$package"]=$evidence
  # dpkg's *.list filenames are always architecture-qualified for some
  # Multi-Arch packages, while ${binary:Package} is not guaranteed to be.
  # Keep both spellings tied to the same package-manifest evidence.
  debian_evidence["${package%%:*}"]=$evidence
done <"$package_manifest_path"

while IFS=$'\t' read -r path hash license evidence _; do
  [[ $path == path ]] && continue
  first_hash["$path"]=$hash
  first_license["$path"]=$license
  first_evidence["$path"]=$evidence
done <"$first_party_path"

while IFS=$'\t' read -r path kind hash target; do
  [[ $path == path ]] && continue
  s6_type["$path"]=$kind
  s6_hash["$path"]=$hash
  s6_target["$path"]=$target
done <"$s6_manifest_path"

classify_exception() {
  local path=$1 pattern class component license evidence source
  while IFS=$'\t' read -r pattern class component license evidence source; do
    [[ -z $pattern || $pattern == \#* ]] && continue
    # Intentional policy glob.
    # shellcheck disable=SC2053
    if [[ $path == $pattern ]]; then
      if [[ $class == volatile-runtime ]]; then
        [[ $evidence == - ]] || { printf 'volatile exception must use evidence -: %s\n' "$path" >&2; return 2; }
      else
        [[ $evidence == /* && -e $(inside_root "$evidence") ]] || {
          printf 'exception evidence is absent for %s: %s\n' "$path" "$evidence" >&2
          return 2
        }
      fi
      printf '%s\t%s\t%s\t%s\t%s\n' "$class" "$component" "$license" "$evidence" "$source"
      return 0
    fi
  done <"$exceptions_path"
  return 1
}

files="$temporary_directory/files"
find "$root" -xdev \
  \( -path "$temporary_directory" -prune \) -o \
  \( -type f -o -type l \) -print0 >"$files"

rows="$temporary_directory/rows"
unknown="$temporary_directory/unknown"
while IFS= read -r -d '' absolute; do
  if [[ $root == / ]]; then path=$absolute; else path=${absolute#"$root"}; fi
  [[ $path != "$output" ]] || continue
  if [[ $path == *$'\n'* || $path == *$'\t'* ]]; then
    printf 'unsupported control character in final image path: %q\n' "$path" >&2
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

  if [[ -n ${first_hash[$path]:-} ]]; then
    [[ $kind == file && $hash == "${first_hash[$path]}" ]] || {
      printf 'first-party file changed after inventory: %s\n' "$path" >&2
      exit 1
    }
    class='first-party'
    component=xenoteer
    license=${first_license[$path]}
    evidence=${first_evidence[$path]}
    source=/usr/share/doc/xenoteer/first-party-files.tsv
  elif [[ -n ${s6_hash[$path]:-} ]]; then
    [[ $kind == "${s6_type[$path]}" && $hash == "${s6_hash[$path]}" ]] || {
      printf 's6 file differs from locked extracted manifest: %s\n' "$path" >&2
      exit 1
    }
    if [[ $kind == symlink && $target != "${s6_target[$path]}" ]]; then
      printf 's6 symlink target differs from locked manifest: %s\n' "$path" >&2
      exit 1
    fi
    class='locked-third-party'
    component=s6-overlay-3.2.2.0
    license=ISC
    evidence=/usr/share/doc/s6-overlay/COPYING
    source=/usr/share/doc/xenoteer/sources.lock
  elif [[ -n ${dpkg_owner[$path]:-} ]]; then
    class=debian-package
    component=${dpkg_owner[$path]}
    license='package-specific'
    first_owner=${component%%,*}
    evidence=${debian_evidence[$first_owner]:-}
    if [[ -z $evidence ]]; then
      unqualified_owner=${first_owner%%:*}
      evidence=${debian_evidence[$unqualified_owner]:-}
    fi
    [[ -n $evidence && -e $(inside_root "$evidence") ]] || {
      printf 'Debian file owner has no copyright evidence: %s (%s; evidence=%s)\n' \
        "$path" "$component" "${evidence:-missing}" >&2
      exit 1
    }
    source=/usr/share/doc/xenoteer/package-manifest.tsv
  elif classification=$(classify_exception "$path"); then
    IFS=$'\t' read -r class component license evidence source <<<"$classification"
    if [[ $class == volatile-runtime ]]; then
      kind='volatile'
      hash=NOASSERTION
      target=-
    fi
  else
    printf '%s\n' "$path" >>"$unknown"
    continue
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$path" "$kind" "$hash" "$target" "$class" "$component" "$license" "$evidence|$source" >>"$rows"
done <"$files"

if [[ -s $unknown ]]; then
  while IFS= read -r path; do
    printf 'unclassified final image file: %s\n' "$path" >&2
  done <"$unknown"
  exit 1
fi

mkdir -p "$(dirname "$output_path")"
{
  printf 'path\ttype\tsha256\tsymlink_target\towner_class\tcomponent\tlicense_expression\tevidence|source\n'
  LC_ALL=C sort -t $'\t' -k1,1 "$rows"
} >"$output_path"
printf 'wrote complete classified final-file inventory: %s\n' "$output"
