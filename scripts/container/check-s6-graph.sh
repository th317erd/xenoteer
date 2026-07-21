#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
graph="$repo_root/container/rootfs/etc/s6-overlay/s6-rc.d"
contents="$graph/user/contents.d"
declare -A present visiting visited

while IFS= read -r -d '' marker; do
  service=${marker##*/}
  present["$service"]=1
done < <(find "$contents" -maxdepth 1 -type f -print0)

if (( ${#present[@]} == 0 )); then
  printf 's6 user bundle is empty\n' >&2
  exit 1
fi

for service in "${!present[@]}"; do
  directory="$graph/$service"
  [[ -d $directory && -f $directory/type ]] || {
    printf 's6 bundle member has no definition: %s\n' "$service" >&2
    exit 1
  }
  service_type=$(<"$directory/type")
  if [[ $(wc -l <"$directory/type") -ne 1 ]]; then
    printf '%s type must contain exactly one line\n' "$service" >&2
    exit 1
  fi
  case "$service_type" in
    longrun)
      [[ -x $directory/run ]] || { printf '%s run is not executable\n' "$service" >&2; exit 1; }
      [[ -x $directory/finish ]] || { printf '%s finish is not executable\n' "$service" >&2; exit 1; }
      [[ -f $directory/notification-fd ]] || { printf '%s has no readiness fd\n' "$service" >&2; exit 1; }
      [[ $(<"$directory/notification-fd") == 3 ]] || {
        printf '%s readiness fd must be 3\n' "$service" >&2
        exit 1
      }
      [[ -f $directory/timeout-up && $(<"$directory/timeout-up") =~ ^[0-9]+$ ]] || {
        printf '%s timeout-up must be numeric\n' "$service" >&2
        exit 1
      }
      [[ -f $directory/timeout-finish && $(<"$directory/timeout-finish") =~ ^[0-9]+$ ]] || {
        printf '%s timeout-finish must be numeric\n' "$service" >&2
        exit 1
      }
      rg -q '(^|[[:space:]])exec([[:space:]]|$)' "$directory/run" || {
        printf '%s run does not exec its supervisor chain\n' "$service" >&2
        exit 1
      }
      ;;
    oneshot)
      [[ -x $directory/up ]] || { printf '%s up is not executable\n' "$service" >&2; exit 1; }
      if [[ -e $directory/down && ! -x $directory/down ]]; then
        printf '%s down is not executable\n' "$service" >&2
        exit 1
      fi
      ;;
    *)
      printf '%s has unsupported s6 type %s\n' "$service" "$service_type" >&2
      exit 1
      ;;
  esac

  if [[ -d $directory/dependencies.d ]]; then
    while IFS= read -r -d '' marker; do
      dependency=${marker##*/}
      if [[ -z ${present[$dependency]:-} ]]; then
        printf '%s depends on service outside user bundle: %s\n' "$service" "$dependency" >&2
        exit 1
      fi
    done < <(find "$directory/dependencies.d" -maxdepth 1 -type f -print0)
  fi
done

visit() {
  local service=$1 dependency marker
  [[ -n ${visited[$service]:-} ]] && return
  if [[ -n ${visiting[$service]:-} ]]; then
    printf 'cycle in s6 graph at %s\n' "$service" >&2
    exit 1
  fi
  visiting["$service"]=1
  if [[ -d $graph/$service/dependencies.d ]]; then
    while IFS= read -r -d '' marker; do
      dependency=${marker##*/}
      visit "$dependency"
    done < <(find "$graph/$service/dependencies.d" -maxdepth 1 -type f -print0)
  fi
  unset 'visiting[$service]'
  visited["$service"]=1
}

for service in "${!present[@]}"; do
  visit "$service"
done

printf 's6 graph valid (%d services)\n' "${#present[@]}"
