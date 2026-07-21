#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

root=${1:-/}
output=${2:-/usr/share/doc/xenoteer/final-files.tsv}
first_party=${3:-/usr/share/doc/xenoteer/first-party-files.tsv}
s6_manifest=${4:-/usr/share/doc/xenoteer/s6-overlay-files.tsv}
exceptions=${5:-/usr/share/xenoteer/final-image-exceptions.tsv}
novnc_manifest=${6:-/usr/share/doc/xenoteer/novnc-files.tsv}
debian_installed_manifest=${7:-/usr/share/doc/xenoteer/debian-installed-files.tsv}
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
novnc_manifest_path=$(inside_root "$novnc_manifest")
debian_installed_manifest_path=$(inside_root "$debian_installed_manifest")
[[ -f $first_party_path ]] || { printf 'first-party image manifest is missing\n' >&2; exit 1; }
[[ -f $s6_manifest_path ]] || { printf 's6 image manifest is missing\n' >&2; exit 1; }
[[ -f $exceptions_path ]] || { printf 'final-image exception policy is missing\n' >&2; exit 1; }
[[ -f $package_manifest_path ]] || { printf 'Debian package manifest is missing\n' >&2; exit 1; }
[[ -f $novnc_manifest_path ]] || { printf 'noVNC extracted-asset manifest is missing\n' >&2; exit 1; }
[[ -f $debian_installed_manifest_path ]] || {
  printf 'post-install Debian filesystem baseline is missing\n' >&2
  exit 1
}
mkdir -p "$(dirname "$output_path")"
install -m 0644 /dev/null "$output_path"

declare -A dpkg_owner debian_evidence first_hash first_license first_evidence first_seen
declare -A s6_type s6_hash s6_target s6_seen
declare -A novnc_type novnc_hash novnc_target novnc_seen
declare -A installed_type installed_hash installed_target installed_mode
declare -A installed_uid installed_gid installed_seen

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
  [[ $path == /* && $hash =~ ^[a-f0-9]{64}$ && -z ${first_hash[$path]:-} ]] || {
    printf 'invalid or duplicate first-party manifest path: %s\n' "$path" >&2
    exit 1
  }
  first_hash["$path"]=$hash
  first_license["$path"]=$license
  first_evidence["$path"]=$evidence
done <"$first_party_path"

while IFS=$'\t' read -r path kind hash target; do
  [[ $path == path ]] && continue
  [[ $path == /* && $kind =~ ^(file|symlink)$ && $hash =~ ^[a-f0-9]{64}$ \
    && -z ${s6_hash[$path]:-} ]] || {
    printf 'invalid or duplicate s6 manifest path: %s\n' "$path" >&2
    exit 1
  }
  s6_type["$path"]=$kind
  s6_hash["$path"]=$hash
  s6_target["$path"]=$target
done <"$s6_manifest_path"

while IFS=$'\t' read -r path kind hash target; do
  [[ $path == path ]] && continue
  [[ $path == /usr/share/novnc/* || $path == /usr/share/doc/novnc/copyright ]] || {
    printf 'noVNC manifest contains an out-of-scope path: %s\n' "$path" >&2
    exit 1
  }
  [[ -z ${novnc_hash[$path]:-} ]] || {
    printf 'duplicate path in noVNC manifest: %s\n' "$path" >&2
    exit 1
  }
  novnc_type["$path"]=$kind
  novnc_hash["$path"]=$hash
  novnc_target["$path"]=$target
done <"$novnc_manifest_path"

while IFS=$'\t' read -r path kind hash target mode uid gid owner verification; do
  [[ $path == path ]] && continue
  [[ $path == /* && $kind =~ ^(file|symlink)$ && $hash =~ ^[a-f0-9]{64}$ \
    && $mode =~ ^[0-7]{3,4}$ && $uid =~ ^[0-9]+$ && $gid =~ ^[0-9]+$ \
    && -n $owner && -n $verification && -z ${installed_hash[$path]:-} ]] || {
    printf 'invalid or duplicate Debian installed-baseline path: %s\n' "$path" >&2
    exit 1
  }
  if [[ $kind == file && $target != - ]]; then
    printf 'installed regular-file baseline has a symlink target: %s\n' "$path" >&2
    exit 1
  fi
  installed_type["$path"]=$kind
  installed_hash["$path"]=$hash
  installed_target["$path"]=$target
  installed_mode["$path"]=$mode
  installed_uid["$path"]=$uid
  installed_gid["$path"]=$gid
done <"$debian_installed_manifest_path"

classify_exception() {
  local path=$1 kind=$2 pattern class component license evidence source
  while IFS=$'\t' read -r pattern class component license evidence source; do
    [[ -z $pattern || $pattern == \#* ]] && continue
    # Intentional policy glob.
    # shellcheck disable=SC2053
    if [[ $path == $pattern ]]; then
      if [[ ( $pattern == *'*'* || $pattern == *'?'* || $pattern == *'['* ) \
          && -z ${installed_hash[$path]:-} ]]; then
        printf 'glob exception matched a path absent from the installed baseline: %s\n' "$path" >&2
        return 2
      fi
      case "$class" in
        generated-symlink)
          if [[ $kind != symlink ]]; then
            printf 'generated-symlink exception matched a non-symlink: %s\n' "$path" >&2
            return 2
          fi
          ;;
        base-image-config|generated-cache|generated-config|generated-data|generated-log|generated-metadata|generated-state|locked-third-party-evidence|volatile-runtime)
          if [[ $kind != file ]]; then
            printf '%s exception matched a non-regular file: %s\n' "$class" "$path" >&2
            return 2
          fi
          ;;
        *)
          printf 'unknown final-image exception class %s for %s\n' "$class" "$path" >&2
          return 2
          ;;
      esac
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

debian_evidence_for() {
  local owner=$1 evidence
  evidence=${debian_evidence[$owner]:-}
  if [[ -z $evidence ]]; then
    evidence=${debian_evidence[${owner%%:*}]:-}
  fi
  [[ -n $evidence ]] || return 1
  printf '%s' "$evidence"
}

# Some maintainer scripts create deterministic files that dpkg deliberately
# does not record in a package *.list. Attribute bytecode to its packaged
# source and update-alternatives links to dpkg without weakening the explicit
# exception policy for unrelated files in those trees.
classify_derived_debian_file() {
  local path=$1 kind=$2 target=$3 source_path owner evidence normalized_target
  local alternative alternatives_state cert_link cert_target package suffix

  # These classes are maintainer/post-install products. A structurally plausible
  # new path added later by COPY is not enough: it must have existed in, and
  # already matched, the exact post-install baseline checked above.
  [[ -n ${installed_hash[$path]:-} ]] || return 1

  if [[ $kind == file \
      && $path =~ ^(.*/)__pycache__/([^/]+)\.cpython-[0-9]+(\.opt-[0-9]+)?\.pyc$ ]]; then
    source_path=${BASH_REMATCH[1]}${BASH_REMATCH[2]}.py
    owner=${dpkg_owner[$source_path]:-}
    [[ -n $owner ]] || return 1
    owner=${owner%%,*}
    evidence=$(debian_evidence_for "$owner") || return 1
    [[ -e $(inside_root "$evidence") ]] || return 1
    printf 'generated-cache\t%s\tpackage-specific\t%s\t%s\n' \
      "$owner" "$evidence" "Python bytecode generated from $source_path"
    return 0
  fi

  if [[ $kind == symlink ]]; then
    if [[ $target == /* ]]; then
      normalized_target=$(realpath -m -s -- "$target")
    else
      normalized_target=$(realpath -m -s -- "$(dirname "$path")/$target")
    fi
    if [[ $normalized_target == /etc/alternatives/* ]]; then
      printf '%s\n' \
        $'generated-symlink\tdpkg\tpackage-specific\t/usr/share/doc/dpkg/copyright\tupdate-alternatives'
      return 0
    fi

    if [[ $path == /etc/alternatives/* ]]; then
      if [[ -e $(inside_root "$normalized_target") \
          || -L $(inside_root "$normalized_target") ]]; then
        printf '%s\n' \
          $'generated-symlink\tdpkg\tpackage-specific\t/usr/share/doc/dpkg/copyright\tupdate-alternatives verified target'
        return 0
      fi

      # Debian slim intentionally removes many manual pages while retaining
      # update-alternatives master and slave links. Admit a dangling
      # alternative only when an exact-baseline state file records its target
      # and either the state filename is the master link name or the state
      # contents name the slave. Baseline identity still locks the symlink.
      for alternatives_state in "$(inside_root /var/lib/dpkg/alternatives)"/*; do
        [[ -f $alternatives_state ]] || continue
        if { [[ ${alternatives_state##*/} == "${path##*/}" ]] \
              || grep -Fxq -- "${path##*/}" "$alternatives_state"; } \
            && grep -Fxq -- "$normalized_target" "$alternatives_state"; then
          printf '%s\n' \
            $'generated-symlink\tdpkg\tpackage-specific\t/usr/share/doc/dpkg/copyright\tupdate-alternatives verified dangling link'
          return 0
        fi
      done
    fi

    if [[ $path == /etc/ssl/certs/*.pem \
        && $target == /usr/share/ca-certificates/* \
        && -e $(inside_root "$target") ]]; then
      printf '%s\n' \
        $'generated-symlink\tca-certificates\tpackage-specific\t/usr/share/doc/ca-certificates/copyright\tupdate-ca-certificates verified PEM link'
      return 0
    fi

    if [[ $path =~ ^/etc/ssl/certs/[a-f0-9]{8}\.[0-9]+$ \
        && $target == *.pem && $target != */* ]]; then
      cert_link=$(inside_root "/etc/ssl/certs/$target")
      if [[ -L $cert_link ]]; then
        cert_target=$(readlink "$cert_link")
        if [[ $cert_target == /usr/share/ca-certificates/* \
            && -e $(inside_root "$cert_target") ]]; then
          printf '%s\n' \
            $'generated-symlink\tca-certificates\tpackage-specific\t/usr/share/doc/ca-certificates/copyright\tupdate-ca-certificates verified subject-hash link'
          return 0
        fi
      fi
    fi

    if [[ $path == /etc/fonts/conf.d/*.conf \
        && $normalized_target == /usr/share/fontconfig/conf.avail/*.conf \
        && ( -e $(inside_root "$normalized_target") \
          || -L $(inside_root "$normalized_target") ) ]]; then
      printf '%s\n' \
        $'generated-symlink\tfontconfig-config\tpackage-specific\t/usr/share/doc/fontconfig-config/copyright\tfontconfig verified target'
      return 0
    fi
  fi

  if [[ $kind == file && $path == /var/lib/dpkg/alternatives/* ]]; then
    alternative=${path##*/}
    if [[ -L $(inside_root "/etc/alternatives/$alternative") ]]; then
      printf '%s\n' \
        $'generated-state\tdpkg\tpackage-specific\t/usr/share/doc/dpkg/copyright\tupdate-alternatives verified state'
      return 0
    fi
  fi

  # dpkg does not list its own per-package database records in each package's
  # payload manifest. Accept only known dpkg record suffixes for a package that
  # is present in the signed package manifest; an arbitrary file placed in this
  # otherwise attractive hiding directory remains unclassified.
  if [[ $kind == file && $path == /var/lib/dpkg/info/format ]]; then
    printf '%s\n' \
      $'generated-metadata\tdpkg\tpackage-specific\t/usr/share/doc/dpkg/copyright\tdpkg database format'
    return 0
  fi
  if [[ $kind == file && $path =~ ^/var/lib/dpkg/info/(.+)\.(conffiles|config|list|md5sums|postinst|postrm|preinst|prerm|shlibs|symbols|templates|triggers)$ ]]; then
    package=${BASH_REMATCH[1]}
    suffix=${BASH_REMATCH[2]}
    evidence=$(debian_evidence_for "$package") || return 1
    [[ -e $(inside_root "$evidence") ]] || return 1
    printf 'generated-metadata\t%s\tpackage-specific\t%s\t%s\n' \
      "$package" "$evidence" "dpkg per-package $suffix record"
    return 0
  fi

  # fc-cache filenames are content-addressed cache records plus one standard
  # cache-directory marker. This is intentionally much narrower than allowing
  # arbitrary content anywhere under /var/cache/fontconfig.
  if [[ $kind == file && $path == /var/cache/fontconfig/CACHEDIR.TAG ]]; then
    printf '%s\n' \
      $'generated-cache\tfontconfig\tpackage-specific\t/usr/share/doc/fontconfig/copyright\tfc-cache directory marker'
    return 0
  fi
  if [[ $kind == file && $path =~ ^/var/cache/fontconfig/[a-f0-9]{32}-le64\.cache-[0-9]+$ ]]; then
    printf '%s\n' \
      $'generated-cache\tfontconfig\tpackage-specific\t/usr/share/doc/fontconfig/copyright\tfc-cache architecture cache'
    return 0
  fi

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
  read -r mode uid gid < <(stat -c '%a %u %g' -- "$absolute")

  if [[ -n ${installed_hash[$path]:-} ]]; then
    installed_seen["$path"]=true
    if [[ $kind != "${installed_type[$path]}" \
        || $hash != "${installed_hash[$path]}" \
        || $target != "${installed_target[$path]}" \
        || $mode != "${installed_mode[$path]}" \
        || $uid != "${installed_uid[$path]}" \
        || $gid != "${installed_gid[$path]}" ]]; then
      printf 'file differs from exact post-install Debian baseline: %s\n' "$path" >&2
      exit 1
    fi
  fi

  if [[ $path == "$output" ]]; then
    hash=SELF-REFERENTIAL
    class=generated-metadata
    component=xenoteer
    license=NOASSERTION
    evidence=/usr/share/doc/xenoteer/NOTICE
    source=inventory-final-image
  elif [[ -n ${first_hash[$path]:-} ]]; then
    [[ $kind == file && $hash == "${first_hash[$path]}" ]] || {
      printf 'first-party file changed after inventory: %s\n' "$path" >&2
      exit 1
    }
    class='first-party'
    component=xenoteer
    license=${first_license[$path]}
    evidence=${first_evidence[$path]}
    source=/usr/share/doc/xenoteer/first-party-files.tsv
    first_seen["$path"]=true
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
    s6_seen["$path"]=true
  elif [[ -n ${novnc_hash[$path]:-} ]]; then
    [[ $kind == "${novnc_type[$path]}" && $hash == "${novnc_hash[$path]}" ]] || {
      printf 'noVNC file differs from locked extracted manifest: %s\n' "$path" >&2
      exit 1
    }
    if [[ $kind == symlink && $target != "${novnc_target[$path]}" ]]; then
      printf 'noVNC symlink target differs from locked manifest: %s\n' "$path" >&2
      exit 1
    fi
    class='locked-third-party'
    component='novnc-1:1.6.0-2'
    license='package-specific'
    evidence=/usr/share/doc/novnc/copyright
    source=/usr/share/doc/xenoteer/sources.lock
    novnc_seen["$path"]=true
  elif [[ -n ${dpkg_owner[$path]:-} ]]; then
    [[ -n ${installed_hash[$path]:-} ]] || {
      printf 'dpkg-owned final path is absent from the verified installed baseline: %s\n' "$path" >&2
      exit 1
    }
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
  elif classification=$(classify_derived_debian_file "$path" "$kind" "$target"); then
    IFS=$'\t' read -r class component license evidence source <<<"$classification"
    [[ $evidence == /* && -e $(inside_root "$evidence") ]] || {
      printf 'derived Debian file has no license evidence: %s (%s)\n' \
        "$path" "$evidence" >&2
      exit 1
    }
  else
    exception_status=0
    classification=$(classify_exception "$path" "$kind") || exception_status=$?
    # A status of 1 means that no exception matched and normal unknown-file
    # reporting should continue. Status 2 means a policy row matched but was
    # internally invalid (wrong file kind, missing evidence, unsafe glob); do
    # not turn that policy violation into a slow end-of-scan unknown-file error.
    if (( exception_status == 0 )); then
      IFS=$'\t' read -r class component license evidence source <<<"$classification"
      if [[ $class == volatile-runtime ]]; then
        kind='volatile'
        hash=NOASSERTION
        target=-
      fi
    elif (( exception_status != 1 )); then
      exit "$exception_status"
    else
      printf '%s\n' "$path" >>"$unknown"
      continue
    fi
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$path" "$kind" "$hash" "$target" "$mode" "$uid" "$gid" \
    "$class" "$component" "$license" "$evidence|$source" >>"$rows"
done <"$files"

for path in "${!first_hash[@]}"; do
  if [[ -z ${first_seen[$path]:-} ]]; then
    printf 'first-party manifest entry is absent from final image: %s\n' "$path" >&2
    exit 1
  fi
done

for path in "${!s6_hash[@]}"; do
  if [[ -z ${s6_seen[$path]:-} ]]; then
    printf 's6 manifest entry is absent from final image: %s\n' "$path" >&2
    exit 1
  fi
done

for path in "${!installed_hash[@]}"; do
  if [[ -z ${installed_seen[$path]:-} ]]; then
    printf 'post-install Debian baseline entry is absent from final image: %s\n' "$path" >&2
    exit 1
  fi
done

for path in "${!novnc_hash[@]}"; do
  if [[ -z ${novnc_seen[$path]:-} ]]; then
    printf 'locked noVNC asset is absent from final image: %s\n' "$path" >&2
    exit 1
  fi
done

if [[ -s $unknown ]]; then
  while IFS= read -r path; do
    printf 'unclassified final image file: %s\n' "$path" >&2
  done <"$unknown"
  exit 1
fi

{
  printf 'path\ttype\tsha256\tsymlink_target\tmode\tuid\tgid\towner_class\tcomponent\tlicense_expression\tevidence|source\n'
  LC_ALL=C sort -t $'\t' -k1,1 "$rows"
} >"$output_path"
printf 'wrote complete classified final-file inventory: %s\n' "$output"
