#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
graph="$repo_root/container/rootfs/etc/s6-overlay/s6-rc.d"
contents="$graph/user/contents.d"
declare -A present visiting visited
declare -A expected_dependencies=(
  [atspi]='session-dbus'
  [critical-shutdown-coordinator]='runtime-directories'
  [desktop-profile]='runtime-directories'
  [machine-id]='runtime-directories'
  [runtime-directories]='shutdown-daemon-ready'
  [session-dbus]='xvfb'
  [shutdown-daemon-ready]=''
  [websockify]='x0tigervnc'
  [x0tigervnc]='xfce'
  [xauthority]='runtime-directories'
  [xenoteerd]='xfce'
  [xfce]='atspi'
  [xvfb]='critical-shutdown-coordinator desktop-profile machine-id xauthority'
)

while IFS= read -r -d '' marker; do
  service=${marker##*/}
  present["$service"]=1
done < <(find "$contents" -maxdepth 1 -type f -print0)

if (( ${#present[@]} == 0 )); then
  printf 's6 user bundle is empty\n' >&2
  exit 1
fi

if (( ${#present[@]} != ${#expected_dependencies[@]} )); then
  printf 's6 user bundle has %d services, expected %d\n' \
    "${#present[@]}" "${#expected_dependencies[@]}" >&2
  exit 1
fi
for service in "${!expected_dependencies[@]}"; do
  if [[ -z ${present[$service]:-} ]]; then
    printf 's6 user bundle is missing required service: %s\n' "$service" >&2
    exit 1
  fi
done

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
      case "$service" in
        websockify|x0tigervnc)
          [[ ! -e $directory/notification-fd ]] || {
            printf '%s must not block the default startup transaction\n' "$service" >&2
            exit 1
          }
          [[ ! -e $directory/timeout-up ]] || {
            printf '%s must not advertise an s6 startup timeout without readiness\n' "$service" >&2
            exit 1
          }
          rg -q "finish-viewer $service" "$directory/finish" || {
            printf '%s does not use the optional/required viewer finish policy\n' "$service" >&2
            exit 1
          }
          ;;
        critical-shutdown-coordinator)
          [[ -f $directory/notification-fd ]] || {
            printf '%s has no readiness fd\n' "$service" >&2
            exit 1
          }
          [[ $(<"$directory/notification-fd") == 3 ]] || {
            printf '%s readiness fd must be 3\n' "$service" >&2
            exit 1
          }
          [[ -f $directory/timeout-up && $(<"$directory/timeout-up") =~ ^[0-9]+$ ]] || {
            printf '%s timeout-up must be numeric\n' "$service" >&2
            exit 1
          }
          [[ ! -e $directory/data/check ]] || {
            printf '%s uses native readiness and must not have a polling check\n' "$service" >&2
            exit 1
          }
          ;;
        *)
          [[ -f $directory/notification-fd ]] || {
            printf '%s has no readiness fd\n' "$service" >&2
            exit 1
          }
          [[ $(<"$directory/notification-fd") == 3 ]] || {
            printf '%s readiness fd must be 3\n' "$service" >&2
            exit 1
          }
          [[ -x $directory/data/check ]] || {
            printf '%s has no executable readiness check\n' "$service" >&2
            exit 1
          }
          [[ -f $directory/timeout-up && $(<"$directory/timeout-up") =~ ^[0-9]+$ ]] || {
            printf '%s timeout-up must be numeric\n' "$service" >&2
            exit 1
          }
          ;;
      esac
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

  actual_dependencies=$(
    if [[ -d $directory/dependencies.d ]]; then
      find "$directory/dependencies.d" -maxdepth 1 -type f -printf '%f\n' \
        | LC_ALL=C sort | paste -sd ' ' -
    fi
  )
  if [[ $actual_dependencies != "${expected_dependencies[$service]}" ]]; then
    printf '%s dependencies were [%s], expected [%s]\n' \
      "$service" "$actual_dependencies" "${expected_dependencies[$service]}" >&2
    exit 1
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

libexec="$repo_root/container/rootfs/usr/local/libexec/xenoteer"
grep -Fq 'dbus-daemon --session --nofork --nopidfile --nosyslog' \
  "$libexec/run-session-dbus"
grep -Fq -- '--address=unix:path=/run/user/1000/bus' \
  "$libexec/run-session-dbus"
grep -Fq 'ATSPI_DBUS_IMPLEMENTATION=dbus-daemon' "$libexec/run-atspi"
grep -Fq '/usr/libexec/at-spi-bus-launcher' "$libexec/run-atspi"
grep -Fq -- '--launch-immediately --a11y=1 --screen-reader=1' "$libexec/run-atspi"
grep -Fq 'xfce4-session --disable-tcp' "$libexec/run-xfce"
grep -Fq -- '-noreset' "$libexec/run-xvfb"
grep -Fq '/readyz' "$libexec/probe-daemon"
grep -Fq '/readyz' "$libexec/healthcheck"
grep -Fq '/command/s6-svstat -o ready /run/service/xenoteerd' "$libexec/healthcheck"
grep -Fq "pgrep -f '^s6-rc .* -u .* -- change top\$'" "$libexec/healthcheck"
if rg -n '/livez' "$libexec/probe-daemon" "$libexec/healthcheck" >/dev/null; then
  printf 'readiness check incorrectly uses the liveness-only endpoint\n' >&2
  exit 1
fi
if rg -n 'Phase 0' "$libexec/init-runtime" >/dev/null; then
  printf 'runtime initialization still contains a stale Phase 0 diagnostic\n' >&2
  exit 1
fi
grep -Fq 'X0tigervnc' "$libexec/run-x0tigervnc"
for flag in '-interface 127.0.0.1' '-localhost=1' '-UseIPv4=1' '-UseIPv6=0' \
  '-SecurityTypes=None' '-AcceptKeyEvents=0' '-AcceptPointerEvents=0' \
  '-AcceptSetDesktopSize=0' '-AcceptCutText=0' '-SendCutText=0' \
  '-SetPrimary=0' '-SendPrimary=0' '-MaxCutText=1024'; do
  grep -Fq -- "$flag" "$libexec/run-x0tigervnc"
done
grep -Fq 'PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1' "$libexec/run-websockify"
grep -Fq 'websockify --web=/usr/share/novnc --heartbeat=30' "$libexec/run-websockify"
grep -Fq '127.0.0.1:6080 127.0.0.1:5900' "$libexec/run-websockify"
grep -Fq 'probe-viewer-protocol' "$libexec/probe-websockify"
grep -Fq 'RFB 003.008' "$libexec/probe-viewer-protocol"
grep -Fq '/usr/local/libexec/xenoteer/probe-xfce' "$libexec/probe-daemon"
grep -Fq '/usr/local/libexec/xenoteer/probe-xfce' "$libexec/healthcheck"
grep -Fq '/command/s6-svwait -U -t 10000 /run/service/s6-linux-init-shutdownd' \
  "$graph/shutdown-daemon-ready/up"
if rg -n 'delay=(16|30)' "$libexec/finish-viewer" >/dev/null; then
  printf 'viewer retry delay exceeds the finish timeout budget\n' >&2
  exit 1
fi
grep -Fq 'critical-shutdown-request' "$libexec/finish-critical"
grep -Fq "/usr/local/libexec/xenoteer/request-critical-shutdown \"\$service\"" \
  "$libexec/run-critical-shutdown-coordinator"
grep -Fq 'exec sleep infinity' "$libexec/run-critical-shutdown-coordinator"
grep -Fq '/command/s6-svwait -D -t 5000' "$libexec/request-critical-shutdown"
grep -Fq '/run/s6/basedir/bin/halt' "$libexec/request-critical-shutdown"
grep -Fq 'made no unlocked s6-rc progress after 5 seconds' \
  "$libexec/request-critical-shutdown"
grep -Fq 'reached its restart ceiling; waiting for explicit operator recovery' \
  "$libexec/finish-viewer"
for service in session-dbus atspi xfce xenoteerd; do
  grep -Fq 'exec s6-setuidgid xenoteer ' "$graph/$service/data/check" || {
    printf '%s readiness check must use the desktop identity\n' "$service" >&2
    exit 1
  }
done

if rg -n '(^|[[:space:]/])(dbus-launch|dbus-run-session|startxfce4)([[:space:]]|$)|dbus-daemon[[:space:]]+--system' \
  "$repo_root/container/rootfs/etc/s6-overlay" "$libexec" >/dev/null; then
  printf 'forbidden competing desktop/session bootstrap command found\n' >&2
  exit 1
fi

printf 's6 graph valid (%d services)\n' "${#present[@]}"
