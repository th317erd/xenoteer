#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -Eeuo pipefail

report_error() {
  local status=$?
  trap - ERR
  printf 'image acceptance failed at line %s (status %s): %s\n' \
    "${BASH_LINENO[0]:-unknown}" "$status" "$BASH_COMMAND" >&2
  exit "$status"
}
trap report_error ERR

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
image_reference=${1:-xenoteer:dev}
image=$(docker image inspect "$image_reference" --format '{{.Id}}')
if [[ ! $image =~ ^sha256:[0-9a-f]{64}$ ]]; then
  printf 'resolved image has an invalid immutable ID: %s\n' "$image" >&2
  exit 1
fi
container_name="xenoteer-image-test-$$"
token_file=$(mktemp)
token_canary='PHASE0_IMAGE_TEST_TOKEN_VALUE_MUST_NEVER_APPEAR_IN_LOGS_0123456789'
created=()

cleanup() {
  local name
  for name in "${created[@]}"; do
    docker rm --force --volumes "$name" >/dev/null 2>&1 || true
  done
  rm -f -- "$token_file"
}
trap cleanup EXIT

printf '%s' "$token_canary" >"$token_file"
chmod 0400 "$token_file"
if [[ $(id -u) -eq 0 ]]; then
  chown 0:0 "$token_file"
elif ! docker info --format '{{json .SecurityOptions}}' | grep -Fq 'name=rootless'; then
  printf 'test-image must run as root or use rootless Docker so the secret maps to container UID 0\n' >&2
  exit 77
fi

start_container() {
  local name=$1
  shift
  docker run --detach \
    --name "$name" \
    --cpus 2 \
    --shm-size=4g \
    --volume "$token_file:/run/secrets/xenoteer_api_token:ro" \
    "$@" \
    "$image" >/dev/null
  created+=("$name")
}

wait_running_probe() {
  local name=$1
  for _ in {1..45}; do
    if docker exec "$name" /usr/local/libexec/xenoteer/healthcheck >/dev/null 2>&1; then
      return 0
    fi
    if [[ $(docker inspect "$name" --format '{{.State.Running}}') != true ]]; then
      docker logs "$name" >&2
      return 1
    fi
    sleep 1
  done
  docker logs "$name" >&2
  printf '%s did not pass the desktop readiness probe\n' "$name" >&2
  return 1
}

wait_stopped() {
  local name=$1
  # The critical finish hook exits 125 while its coordinator requests overlay
  # halt. Keep a bounded margin for the outer s6-rc shutdown transaction and its
  # five-second no-progress fallback.
  for _ in {1..50}; do
    [[ $(docker inspect "$name" --format '{{.State.Running}}') == false ]] && return 0
    sleep 1
  done
  docker logs "$name" >&2
  printf '%s did not stop after critical service exit\n' "$name" >&2
  return 1
}

assert_logs_exclude() {
  local name=$1 forbidden=$2 description=$3
  if logs_contain "$name" "$forbidden"; then
    printf '%s logs exposed %s\n' "$name" "$description" >&2
    exit 1
  fi
}

logs_contain() {
  local name=$1 expected=$2 output
  output=$(docker logs "$name" 2>&1)
  grep -Fq -- "$expected" <<<"$output"
}

assert_logs_contain() {
  local name=$1 expected=$2 description=$3
  if logs_contain "$name" "$expected"; then
    return 0
  fi
  docker logs "$name" >&2
  printf '%s logs did not contain %s\n' "$name" "$description" >&2
  return 1
}

allowed_critical_claimants() {
  case "$1" in
    xvfb) printf '%s\n' 'xvfb session-dbus atspi xfce xenoteer-processd xenoteerd' ;;
    session-dbus) printf '%s\n' 'session-dbus atspi xfce xenoteer-processd xenoteerd' ;;
    atspi) printf '%s\n' 'atspi xfce xenoteer-processd xenoteerd' ;;
    xfce) printf '%s\n' 'xfce xenoteer-processd xenoteerd' ;;
    xenoteer-processd) printf '%s\n' 'xenoteer-processd xenoteerd' ;;
    xenoteerd) printf '%s\n' 'xenoteerd' ;;
    x0tigervnc) printf '%s\n' 'x0tigervnc websockify' ;;
    websockify) printf '%s\n' 'websockify' ;;
    *) printf 'unknown critical trigger: %s\n' "$1" >&2; return 1 ;;
  esac
}

assert_critical_shutdown() {
  local name=$1 trigger=$2 profile=$3 logs claimants claimant claimant_count allowed accepted
  logs=$(docker logs "$name" 2>&1)
  claimants=$(sed -n \
    's/^critical service \([a-z0-9-]*\) exited unexpectedly; container exit result .*/\1/p' \
    <<<"$logs")
  claimant_count=$(awk 'NF { count++ } END { print count + 0 }' <<<"$claimants")
  if [[ $claimant_count -ne 1 ]]; then
    printf '%s\n' "$logs" >&2
    printf '%s %s trigger produced %s critical shutdown claimants, expected one\n' \
      "$name" "$profile" "$claimant_count" >&2
    return 1
  fi
  claimant=$claimants
  allowed=$(allowed_critical_claimants "$trigger")
  case " $allowed " in
    *" $claimant "*) ;;
    *)
      printf '%s\n' "$logs" >&2
      printf '%s %s trigger was claimed by impossible service %s; allowed: %s\n' \
        "$trigger" "$profile" "$claimant" "$allowed" >&2
      return 1
      ;;
  esac
  accepted=$(grep -Fc 'critical container shutdown request accepted on attempt' \
    <<<"$logs" || true)
  if [[ $accepted -ne 1 ]]; then
    printf '%s\n' "$logs" >&2
    printf '%s %s trigger produced %s accepted shutdown requests, expected one\n' \
      "$trigger" "$profile" "$accepted" >&2
    return 1
  fi
}

assert_loopback_listener() {
  local name=$1 port_hex=$2 description=$3
  docker exec "$name" awk -v endpoint="0100007F:$port_hex" '
    NR > 1 && $4 == "0A" && $2 == endpoint { count++ }
    END { exit count == 1 ? 0 : 1 }
  ' /proc/net/tcp || {
    printf '%s has no single IPv4 loopback %s listener\n' "$name" "$description" >&2
    return 1
  }
  docker exec "$name" awk -v suffix=":$port_hex" '
    NR > 1 && $4 == "0A" && substr($2, length($2) - length(suffix) + 1) == suffix {
      found=1
    }
    END { exit found ? 1 : 0 }
  ' /proc/net/tcp6 || {
    printf '%s unexpectedly exposes an IPv6 %s listener\n' "$name" "$description" >&2
    return 1
  }
  assert_udp_listener_absent "$name" "$port_hex" "$description"
}

assert_udp_listener_absent() {
  local name=$1 port_hex=$2 description=$3
  docker exec "$name" awk -v suffix=":$port_hex" '
    FNR > 1 && substr($2, length($2) - length(suffix) + 1) == suffix { found=1 }
    END { exit found ? 1 : 0 }
  ' /proc/net/udp /proc/net/udp6 || {
    printf '%s unexpectedly has a UDP %s listener\n' "$name" "$description" >&2
    return 1
  }
}

assert_listener_absent() {
  local name=$1 port_hex=$2 description=$3
  docker exec "$name" awk -v suffix=":$port_hex" '
    NR > 1 && $4 == "0A" && substr($2, length($2) - length(suffix) + 1) == suffix {
      found=1
    }
    END { exit found ? 1 : 0 }
  ' /proc/net/tcp /proc/net/tcp6 || {
    printf '%s unexpectedly has a %s listener\n' "$name" "$description" >&2
    return 1
  }
  assert_udp_listener_absent "$name" "$port_hex" "$description"
}

kill_service_payload() {
  local name=$1 service=$2
  case "$service" in
    xvfb) docker exec "$name" pkill -TERM -x Xvfb ;;
    xenoteerd) docker exec "$name" pkill -TERM -x xenoteerd ;;
    xenoteer-processd) docker exec "$name" pkill -TERM -x xenoteer-proces ;;
    session-dbus)
      docker exec "$name" sh -eu -c '
        pid=$(pgrep -u 1000 -f "^dbus-daemon --session --nofork --nopidfile --nosyslog --address=unix:path=/run/xenoteer/bus/session$")
        test "$(printf "%s\n" "$pid" | wc -l)" -eq 1
        kill -TERM "$pid"
      '
      ;;
    atspi)
      docker exec "$name" pkill -TERM -f \
        '^/usr/libexec/at-spi-bus-launcher --launch-immediately --a11y=1 --screen-reader=1$'
      ;;
    xfce) docker exec "$name" pkill -TERM -x xfce4-session ;;
    x0tigervnc) docker exec "$name" pkill -TERM -x X0tigervnc ;;
    websockify) docker exec "$name" pkill -TERM -f \
      'websockify --web=/usr/share/novnc --heartbeat=30 127.0.0.1:6080 127.0.0.1:5900$'
      ;;
    *) printf 'unknown service test target: %s\n' "$service" >&2; return 1 ;;
  esac
}

assert_zero_payload_capabilities() {
  local name=$1
  docker exec "$name" sh -eu -c '
    matched=0
    for status in /proc/[0-9]*/status; do
      test -r "$status" || continue
      uid=$(awk '\''$1 == "Uid:" { print $2 }'\'' "$status")
      case "$uid" in 1000|1001) ;; *) continue ;; esac
      pid=${status#/proc/}
      pid=${pid%/status}
      process=$(cat "/proc/$pid/comm" 2>/dev/null || printf disappeared)
      matched=$((matched + 1))
      for field in CapInh CapPrm CapEff CapAmb; do
        value=$(awk -v wanted="$field:" '\''$1 == wanted { print $2 }'\'' "$status")
        case "$value" in
          ""|*[!0]*)
            printf "%s PID %s retained %s=%s under hardened runtime\n" \
              "$process" "$pid" "$field" "${value:-missing}" >&2
            exit 1
            ;;
          *) ;;
        esac
      done
    done
    test "$matched" -gt 0
  '
}

# Stop before readiness while the s6 startup transaction is in flight. Wait
# only for PID 1 to become s6-svscan and its signal handler to exist: a signal
# sent earlier can land in the unavoidable kernel-to-/init exec window and is
# not a service-graph shutdown test.
startup_stop="${container_name}-startup-stop"
start_container "$startup_stop"
test "$(docker inspect "$startup_stop" --format '{{.State.Health.Status}}')" = starting
startup_transaction_observed=0
for _ in {1..200}; do
  if docker exec "$startup_stop" sh -c '
    test "$(cat /proc/1/comm)" = s6-svscan
    pgrep -x s6-rc >/dev/null
    test "$(curl --silent --output /dev/null --write-out "%{http_code}" http://127.0.0.1:8080/readyz 2>/dev/null || true)" != 200
  ' >/dev/null 2>&1; then
    startup_transaction_observed=1
    break
  fi
  sleep 0.01
done
test "$startup_transaction_observed" -eq 1
# The root-only maintenance escape hatch is for the explicit AT-SPI recovery
# gate only. A global stop must be recognized from s6's requested graph-down
# intent, without a pre-created per-service exception.
docker exec "$startup_stop" test ! -e /run/xenoteer/critical-maintenance
docker stop --time 35 "$startup_stop" >/dev/null
test "$(docker inspect "$startup_stop" --format '{{.State.ExitCode}}')" -eq 0
if logs_contain "$startup_stop" 'exited unexpectedly'; then
  printf 'immediate startup stop was classified as a critical crash\n' >&2
  exit 1
fi

# Crash XFCE while the upward s6-rc transaction still owns its lock. The
# shutdown daemon accepts the orderly request, but rc.shutdown cannot acquire
# that lock because daemon readiness can no longer complete. The coordinator's
# progress watchdog must terminate the tree well before the 65-second startup
# timeout, retain a nonzero result, and prevent a respawn.
startup_crash="${container_name}-startup-crash"
start_container "$startup_crash"
startup_crash_window=0
startup_transition_pid=
for _ in {1..400}; do
  startup_transition_pid=$(docker exec "$startup_crash" \
    pgrep -f '^s6-rc .* -u .* -- change top$' 2>/dev/null || true)
  if [[ -n $startup_transition_pid ]] \
    && docker exec "$startup_crash" pgrep -x xenoteerd >/dev/null 2>&1; then
    startup_crash_window=1
    break
  fi
  sleep 0.05
done
test "$startup_crash_window" -eq 1

# Hold the exact upward transaction after it has launched the daemon, then wait
# for all functional probes to pass. Docker health must remain false solely
# because the transaction still owns the s6-rc lock.
docker exec "$startup_crash" kill -STOP "$startup_transition_pid"
startup_functional=0
for _ in {1..100}; do
  if docker exec "$startup_crash" /command/s6-envdir -f -L \
    /run/xenoteer/env /command/s6-setuidgid xenoteer \
    /usr/local/libexec/xenoteer/probe-daemon >/dev/null 2>&1; then
    startup_functional=1
    break
  fi
  sleep 0.1
done
test "$startup_functional" -eq 1
if docker exec "$startup_crash" /usr/local/libexec/xenoteer/healthcheck \
  >/dev/null 2>&1; then
  printf 'Docker health became ready while the upward s6-rc lock was held\n' >&2
  exit 1
fi
kill_service_payload "$startup_crash" xfce
wait_stopped "$startup_crash"
test "$(docker inspect "$startup_crash" --format '{{.State.ExitCode}}')" -ne 0
assert_critical_shutdown "$startup_crash" xfce startup
assert_logs_contain "$startup_crash" \
  'orderly critical shutdown was accepted but made no unlocked s6-rc progress after 5 seconds' \
  'the upward-transaction shutdown fallback diagnostic'
assert_logs_exclude "$startup_crash" "$token_canary" 'authentication token contents'

start_container "$container_name" \
  --env XENOTEER__VIEWER__ENABLED=true \
  --env 'XENOTEER__VIEWER__ALLOWED_ORIGINS=["http://127.0.0.1:8080"]'
wait_running_probe "$container_name"

test "$(docker inspect "$container_name" --format '{{json .Config.Entrypoint}}')" = '["/init"]'
test "$(docker exec "$container_name" cat /proc/1/comm)" = s6-svscan
docker exec "$container_name" sh -eu -c '
  test "$(stat -c %u /proc/$(pgrep -xo Xvfb))" = 1000
  daemon_pid=$(pgrep -xo xenoteerd)
  broker_pid=$(pgrep -xo xenoteer-proces)
  test "$(stat -c %u /proc/$daemon_pid)" = 1001
  test "$(stat -c %u:%g /proc/$broker_pid)" = 0:0
  test "$(id -g xenoteerd)" = 1001
  id -G xenoteerd | tr " " "\n" | grep -Fx 1000 >/dev/null
  test "$(stat -c %a:%u:%g /run/secrets/xenoteer_api_token)" = 400:0:0
  test ! -e /run/xenoteer/api-token
  test ! -e /run/xenoteer/api-token-pipe
  # Descriptor numbers are reusable. Prove the token FIFO description is gone
  # without requiring its former slot 9 to remain permanently unallocated.
  test -z "$(/command/s6-setuidgid xenoteerd find -L "/proc/$daemon_pid/fd" \
    -mindepth 1 -maxdepth 1 ! -name 0 ! -name 1 ! -name 2 \
    -type p -print -quit)"
  grep -Eq "^Max core file size[[:space:]]+0[[:space:]]+0" "/proc/$daemon_pid/limits"
  test "$(stat -c %a:%u:%g /run/user/1000)" = 700:1000:1000
  test "$(stat -c %a:%u:%g /run/user/1000/at-spi)" = 710:1000:1000
  test "$(stat -c %a:%u:%g /run/user/1000/Xauthority)" = 600:1000:1000
  test "$(stat -c %a /run/user/1000/ICEauthority)" = 600
  test "$(stat -c %a:%u:%g /run/user/1001)" = 700:1001:1001
  test "$(stat -c %a:%u:%g /run/user/1001/Xauthority)" = 600:1001:1001
  test "$(stat -c %a:%u:%g /run/user/1001/home)" = 700:1001:1001
  test "$(stat -c %a:%u:%g /run/user/1001/xdg/config)" = 700:1001:1001
  test "$(stat -c %a:%u:%g /run/user/1001/xdg/cache)" = 700:1001:1001
  test "$(stat -c %a:%u:%g /run/user/1001/xdg/data)" = 700:1001:1001
  test "$(stat -c %a:%u:%g /run/xenoteer/artifacts)" = 700:1001:1001
  test "$(stat -c %a:%u:%g /run/xenoteer/processd)" = 750:0:1001
  test "$(stat -c %a:%u:%g /run/xenoteer/processd/broker.sock)" = 660:0:1001
  /command/s6-setuidgid xenoteerd /usr/local/bin/xenoteer-processd --probe
  ! /command/s6-setuidgid xenoteer /usr/local/bin/xenoteer-processd --probe \
    >/dev/null 2>&1
  test "$(stat -c %a /tmp/.X11-unix)" = 1777
  test "$(stat -c %u:%g /tmp/.X11-unix)" = 0:0
  test "$(stat -c %a /tmp/.ICE-unix)" = 1777
  test "$(stat -c %u:%g /tmp/.ICE-unix)" = 0:0
  test "$(stat -c %a:%u:%g /run/user/1000/xdg/config)" = 700:1000:1000
  test "$(stat -c %a:%u:%g /run/user/1000/xdg/cache)" = 700:1000:1000
  test "$(stat -c %a:%u:%g /run/user/1000/xdg/data)" = 700:1000:1000
  test -p /run/xenoteer/critical-shutdown-request
  test "$(stat -c %a:%u:%g /run/xenoteer/critical-shutdown-request)" = 600:0:0
  test "$(pgrep -f -c "^/bin/sh /usr/local/libexec/xenoteer/run-critical-shutdown-coordinator$")" -eq 1
  test "$(cat /run/xenoteer/shm-bytes)" -ge 4294967296
  test "$(cat /run/xenoteer/env/XVFB_SCREEN_GEOMETRY)" = 1920x1080x24
  DISPLAY=:99 XAUTHORITY=/run/user/1000/Xauthority xdpyinfo \
    | grep -F "dimensions:    1920x1080 pixels" >/dev/null
  DISPLAY=:99 XAUTHORITY=/run/user/1000/Xauthority xdpyinfo \
    | grep -F "resolution:    96x96 dots per inch" >/dev/null
  ! env -u XAUTHORITY HOME=/tmp xdpyinfo -display :99 >/dev/null 2>&1
  ! awk "NR > 1 && \$2 ~ /:17D3$/ { found=1 } END { exit found ? 0 : 1 }" /proc/net/tcp /proc/net/tcp6
  curl --fail --silent --show-error http://127.0.0.1:8080/livez >/dev/null
  test "$(curl --silent --output /dev/null --write-out "%{http_code}" http://127.0.0.1:8080/readyz)" = 200
  test "$(pgrep -u 1000 -x Xvfb | wc -l)" -eq 1
  test "$(pgrep -u 1001 -x xenoteerd | wc -l)" -eq 1
  test "$(pgrep -u 1000 -x xfce4-session | wc -l)" -eq 1
  test "$(pgrep -u 1000 -x xfwm4 | wc -l)" -eq 1
  test "$(pgrep -u 1000 -x xfsettingsd | wc -l)" -eq 1
  test "$(pgrep -u 1000 -x xfdesktop | wc -l)" -eq 1
  test "$(pgrep -u 1000 -x X0tigervnc | wc -l)" -eq 1
  test "$(pgrep -u 1000 -f "websockify --web=/usr/share/novnc --heartbeat=30 127.0.0.1:6080 127.0.0.1:5900$" | wc -l)" -eq 1
  test "$(pgrep -u 1000 -f "^/usr/libexec/at-spi-bus-launcher --launch-immediately --a11y=1 --screen-reader=1$" | wc -l)" -eq 1
  test "$(pgrep -u 1000 -x at-spi2-registr | wc -l)" -eq 1
  test "$(pgrep -u 1000 -x dbus-daemon | wc -l)" -eq 2
  test "$(pgrep -u 1000 -f "^dbus-daemon --session --nofork --nopidfile --nosyslog --address=unix:path=/run/xenoteer/bus/session$" | wc -l)" -eq 1
  test -S /run/xenoteer/bus/session
  test -S /run/xenoteer/bus/at-spi/bus_99
  test "$(stat -c %a:%u:%g /run/xenoteer/bus)" = 710:1000:1000
  test "$(stat -c %a:%u:%g /run/xenoteer/bus/at-spi)" = 710:1000:1000
  test ! -S /run/dbus/system_bus_socket
  ! pgrep -u 1000 -x xfce4-panel >/dev/null
  ! pgrep -u 1000 -x Thunar >/dev/null
  ! pgrep -u 1000 -x s6-pause >/dev/null
  ! pgrep -f "(^|/)dbus-[l]aunch([[:space:]]|$)" >/dev/null
  ! pgrep -f "(^|/)star[t]xfce4([[:space:]]|$)" >/dev/null
  /command/s6-setuidgid xenoteer gdbus call --address unix:path=/run/xenoteer/bus/session \
    --dest org.a11y.Bus --object-path /org/a11y/bus \
    --method org.freedesktop.DBus.Properties.Get org.a11y.Status IsEnabled \
    | grep -Fx "(<true>,)" >/dev/null
  /command/s6-setuidgid xenoteer gdbus call --address unix:path=/run/xenoteer/bus/at-spi/bus_99 \
    --dest org.a11y.atspi.Registry --object-path /org/a11y/atspi/registry \
    --method org.a11y.atspi.Registry.GetRegisteredEvents >/dev/null
  /command/s6-setuidgid xenoteerd env DISPLAY=:99 \
    XAUTHORITY=/run/user/1001/Xauthority xdpyinfo \
    | grep -F "dimensions:    1920x1080 pixels" >/dev/null
  /command/s6-setuidgid xenoteerd gdbus call --address unix:path=/run/xenoteer/bus/session \
    --dest org.a11y.Bus --object-path /org/a11y/bus \
    --method org.freedesktop.DBus.Properties.Get org.a11y.Status IsEnabled \
    | grep -Fx "(<true>,)" >/dev/null
  /command/s6-setuidgid xenoteerd gdbus call --address unix:path=/run/xenoteer/bus/at-spi/bus_99 \
    --dest org.a11y.atspi.Registry --object-path /org/a11y/atspi/registry \
    --method org.a11y.atspi.Registry.GetRegisteredEvents >/dev/null
  atspi_children=$(/command/s6-setuidgid xenoteerd gdbus call \
    --address unix:path=/run/xenoteer/bus/at-spi/bus_99 \
    --dest org.a11y.atspi.Registry \
    --object-path /org/a11y/atspi/accessible/root \
    --method org.a11y.atspi.Accessible.GetChildren)
  first_atspi_app=$(printf "%s\n" "$atspi_children" \
    | sed -n "s/.*\(:[0-9][0-9.]*\)[^,]*, objectpath.*/\1/p")
  test -n "$first_atspi_app"
  /command/s6-setuidgid xenoteerd gdbus call \
    --address unix:path=/run/xenoteer/bus/at-spi/bus_99 \
    --dest "$first_atspi_app" \
    --object-path /org/a11y/atspi/accessible/root \
    --method org.freedesktop.DBus.Properties.Get \
    org.a11y.atspi.Accessible Name \
    | grep -Eq "^\(<.{3,}>,\)$"
  # Arbitrary XFCE applications may legitimately return an empty private
  # Application bus address. The controlled GTK3 Phase 5 fixture separately
  # proves that UID 1001 cannot connect when an application exposes one.
  ! gdbus call --address unix:path=/run/xenoteer/bus/session \
    --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
    --method org.freedesktop.DBus.ListNames >/dev/null 2>&1
  ! gdbus call --address unix:path=/run/xenoteer/bus/at-spi/bus_99 \
    --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
    --method org.freedesktop.DBus.ListNames >/dev/null 2>&1
  tail -n +2 /usr/share/doc/xenoteer/package-manifest.tsv | LC_ALL=C sort -c
  test -s /usr/share/doc/xenoteer/first-party-files.tsv
  test -s /usr/share/doc/xenoteer/final-files.tsv
  test -s /usr/share/doc/xenoteer/cargo-components.tsv
  test -s /usr/share/doc/xenoteer/cargo-components.spdx.json
  grep -Eq "^xenoteerd[[:space:]]" /usr/share/doc/xenoteer/cargo-components.tsv
  grep -Eq "^tokio[[:space:]]" /usr/share/doc/xenoteer/cargo-components.tsv
  grep -Eq "^/init[[:space:]].*[[:space:]]locked-third-party[[:space:]]s6-overlay-3\\.2\\.2\\.0[[:space:]]ISC[[:space:]]" \
    /usr/share/doc/xenoteer/final-files.tsv
  grep -Eq "^/usr/share/doc/xenoteer/cargo-components\\.tsv[[:space:]].*[[:space:]]generated-metadata[[:space:]]xenoteerd-cargo-closure[[:space:]]" \
    /usr/share/doc/xenoteer/final-files.tsv
'
docker exec "$container_name" /command/s6-setuidgid xenoteer sh -eu -c '
  daemon_pid=$(pgrep -xo xenoteerd)
  test ! -r /run/secrets/xenoteer_api_token
  test ! -e /run/xenoteer/api-token
  test ! -r "/proc/$daemon_pid/fd/9"
  test ! -r "/proc/$daemon_pid/environ"
  test ! -r "/proc/$daemon_pid/mem"
  test ! -r /run/user/1001/Xauthority
  test ! -x /run/user/1001
'
assert_loopback_listener "$container_name" 170C 'RFB'
assert_loopback_listener "$container_name" 17C0 'WebSocket/noVNC'
"$repo_root/scripts/container/assert-idle-runtime.sh" "$container_name" bare 1
"$repo_root/scripts/container/test-viewer-denial.sh" "$container_name"

# Recurring health must reject stopped required XFCE children without relying
# on process absence, then recover with the identical process once continued.
for process in xfwm4 xfsettingsd xfdesktop; do
  process_pid=$(docker exec "$container_name" pgrep -xo "$process")
  docker exec "$container_name" kill -STOP "$process_pid"
  stopped_rejected=0
  if ! docker exec "$container_name" /usr/local/libexec/xenoteer/healthcheck \
    >/dev/null 2>&1; then
    stopped_rejected=1
  fi
  docker exec "$container_name" kill -CONT "$process_pid"
  test "$stopped_rejected" -eq 1
  for _ in {1..30}; do
    docker exec "$container_name" /usr/local/libexec/xenoteer/healthcheck \
      >/dev/null 2>&1 && break
    sleep 0.1
  done
  docker exec "$container_name" /usr/local/libexec/xenoteer/healthcheck >/dev/null
  test "$(docker exec "$container_name" pgrep -xo "$process")" = "$process_pid"
done

# The closed forbidden-process set is also recurring, so a post-readiness
# resurrection is detected and clears once the offending process is removed.
docker exec --detach "$container_name" /command/s6-setuidgid xenoteer sleep 9999
forbidden_rejected=0
if ! docker exec "$container_name" /usr/local/libexec/xenoteer/healthcheck \
  >/dev/null 2>&1; then
  forbidden_rejected=1
fi
docker exec "$container_name" pkill -TERM -u 1000 -f '^sleep 9999$'
test "$forbidden_rejected" -eq 1
for _ in {1..30}; do
  docker exec "$container_name" /usr/local/libexec/xenoteer/healthcheck \
    >/dev/null 2>&1 && break
  sleep 0.1
done
docker exec "$container_name" /usr/local/libexec/xenoteer/healthcheck >/dev/null

docker exec "$container_name" cat /usr/share/doc/xenoteer/cargo-components.spdx.json \
  | jq -e '.spdxVersion == "SPDX-2.3" and ([.packages[].name] | index("xenoteerd") != null)' \
  >/dev/null
assert_logs_exclude "$container_name" "$token_canary" 'authentication token contents'
assert_logs_exclude "$container_name" 'Failed to create peer' \
  'a forbidden cross-UID AT-SPI P2P attempt'

for _ in {1..45}; do
  health=$(docker inspect "$container_name" --format '{{.State.Health.Status}}')
  [[ $health == healthy ]] && break
  sleep 1
done
test "$(docker inspect "$container_name" --format '{{.State.Health.Status}}')" = healthy

started=$(date +%s)
docker stop --time 35 "$container_name" >/dev/null
elapsed=$(($(date +%s) - started))
if (( elapsed > 35 )); then
  printf 'clean stop exceeded 35 seconds: %ss\n' "$elapsed" >&2
  exit 1
fi
test "$(docker inspect "$container_name" --format '{{.State.ExitCode}}')" -eq 0
if logs_contain "$container_name" 'exited unexpectedly'; then
  printf 'healthy operator stop was classified as a critical crash\n' >&2
  exit 1
fi

hardened="${container_name}-hardened"
hardened_args=(
  --read-only
  --cap-drop ALL
  --cap-add CHOWN
  --cap-add DAC_OVERRIDE
  --cap-add FOWNER
  --cap-add KILL
  --cap-add SETGID
  --cap-add SETUID
  --cap-add SYS_CHROOT
  --security-opt no-new-privileges:true
  --pids-limit 512
  --memory 6g
  --tmpfs '/run:rw,nosuid,nodev,exec,size=64m,mode=0755'
  --tmpfs '/tmp:rw,nosuid,nodev,noexec,size=1g,mode=1777'
  --volume /home/xenoteer
  --volume /workspace
)
start_container "$hardened" "${hardened_args[@]}"
wait_running_probe "$hardened"
test "$(docker inspect "$hardened" --format '{{.HostConfig.ReadonlyRootfs}}')" = true
test "$(docker inspect "$hardened" --format '{{index .HostConfig.SecurityOpt 0}}')" = no-new-privileges:true
docker inspect "$hardened" --format '{{json .HostConfig.CapAdd}}' \
  | jq -e 'sort == ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_KILL", "CAP_SETGID", "CAP_SETUID", "CAP_SYS_CHROOT"]' \
  >/dev/null
assert_zero_payload_capabilities "$hardened"
docker stop --time 35 "$hardened" >/dev/null
test "$(docker inspect "$hardened" --format '{{.State.ExitCode}}')" -eq 0
assert_logs_exclude "$hardened" "$token_canary" 'authentication token contents'
if logs_contain "$hardened" 'exited unexpectedly'; then
  printf 'healthy hardened stop was classified as a critical crash\n' >&2
  exit 1
fi

# Negative capability proof: derive the otherwise exact hardened profile by
# deleting only CAP_KILL. PID 1 can still boot, but cannot gracefully signal the
# UID-1000 payloads, so Docker must enforce the short stop deadline with SIGKILL.
hardened_without_kill_args=()
for ((i = 0; i < ${#hardened_args[@]}; i++)); do
  if [[ ${hardened_args[i]} == --cap-add && ${hardened_args[i + 1]:-} == KILL ]]; then
    i=$((i + 1))
    continue
  fi
  hardened_without_kill_args+=("${hardened_args[i]}")
done
without_kill="${container_name}-hardened-without-kill"
start_container "$without_kill" "${hardened_without_kill_args[@]}"
wait_running_probe "$without_kill"
docker inspect "$without_kill" --format '{{json .HostConfig.CapAdd}}' \
  | jq -e 'sort == ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_SETGID", "CAP_SETUID", "CAP_SYS_CHROOT"]' \
  >/dev/null
docker exec "$without_kill" sh -eu -c '
  test "$(stat -c %u /proc/$(pgrep -xo Xvfb))" = 1000
  test "$(stat -c %u /proc/$(pgrep -xo xenoteerd))" = 1001
'
started_ms=$(date +%s%3N)
docker stop --time 5 "$without_kill" >/dev/null
elapsed_ms=$(($(date +%s%3N) - started_ms))
test "$(docker inspect "$without_kill" --format '{{.State.ExitCode}}')" -eq 137
if (( elapsed_ms > 12000 )); then
  printf 'CAP_KILL negative stop proof exceeded its bound: %sms\n' "$elapsed_ms" >&2
  exit 1
fi

logging="${container_name}-logging"
logging_filter='info,xenoteerd=trace'
start_container "$logging" --env "XENOTEER__LOGGING__FILTER=$logging_filter"
wait_running_probe "$logging"
logging_output=$(docker logs "$logging" 2>&1)
grep -Fq 'loaded validated configuration' <<<"$logging_output"
grep -Fq "$logging_filter" <<<"$logging_output"
assert_logs_exclude "$logging" "$token_canary" 'authentication token contents'
docker stop --time 35 "$logging" >/dev/null

malformed="${container_name}-malformed-env"
malformed_value='PHASE0_MALFORMED_ENV_VALUE_MUST_NOT_LEAK'
start_container "$malformed" --env "XENOTEER_BAD=$malformed_value"
wait_stopped "$malformed"
test "$(docker inspect "$malformed" --format '{{.State.ExitCode}}')" -ne 0
malformed_output=$(docker logs "$malformed" 2>&1)
grep -Fq 'invalid Xenoteer environment configuration key' <<<"$malformed_output"
assert_logs_exclude "$malformed" "$malformed_value" 'malformed environment value'
assert_logs_exclude "$malformed" "$token_canary" 'authentication token contents'

strict_loader="${container_name}-strict-loader"
strict_value='PHASE0_TYPED_UNKNOWN_VALUE_MUST_NOT_LEAK'
start_container "$strict_loader" --env "XENOTEER__UNKNOWN__FIELD=$strict_value"
wait_stopped "$strict_loader"
test "$(docker inspect "$strict_loader" --format '{{.State.ExitCode}}')" -ne 0
strict_output=$(docker logs "$strict_loader" 2>&1)
grep -Fq 'xenoteerd startup failed: configuration shape is invalid: unknown field' \
  <<<"$strict_output"
test "$(grep -Fc 'xenoteerd startup failed:' <<<"$strict_output")" -eq 1
assert_logs_exclude "$strict_loader" "$strict_value" 'typed environment value'
assert_logs_exclude "$strict_loader" "$token_canary" 'authentication token contents'

strict_hardened="${container_name}-strict-loader-hardened"
start_container "$strict_hardened" "${hardened_args[@]}" \
  --env "XENOTEER__UNKNOWN__FIELD=$strict_value"
wait_stopped "$strict_hardened"
test "$(docker inspect "$strict_hardened" --format '{{.State.ExitCode}}')" -ne 0
strict_hardened_output=$(docker logs "$strict_hardened" 2>&1)
grep -Fq 'xenoteerd startup failed: configuration shape is invalid: unknown field' \
  <<<"$strict_hardened_output"
test "$(grep -Fc 'xenoteerd startup failed:' <<<"$strict_hardened_output")" -eq 1
assert_logs_exclude "$strict_hardened" "$strict_value" 'typed environment value'
assert_logs_exclude "$strict_hardened" "$token_canary" 'authentication token contents'

empty_typed="${container_name}-empty-typed"
start_container "$empty_typed" --env XENOTEER__DESKTOP__DISPLAY_WIDTH=
wait_stopped "$empty_typed"
test "$(docker inspect "$empty_typed" --format '{{.State.ExitCode}}')" -ne 0
empty_output=$(docker logs "$empty_typed" 2>&1)
grep -Fq 'xenoteerd startup failed: configuration shape is invalid: incompatible value type' \
  <<<"$empty_output"
test "$(grep -Fc 'xenoteerd startup failed:' <<<"$empty_output")" -eq 1
assert_logs_exclude "$empty_typed" "$token_canary" 'authentication token contents'

wrong_geometry="${container_name}-wrong-geometry"
start_container "$wrong_geometry" --env XVFB_SCREEN_WIDTH=1280
wait_stopped "$wrong_geometry"
test "$(docker inspect "$wrong_geometry" --format '{{.State.ExitCode}}')" -ne 0
geometry_output=$(docker logs "$wrong_geometry" 2>&1)
grep -Fq 'Phase 2 requires fixed Xvfb geometry 1920x1080x24 at 96 DPI' \
  <<<"$geometry_output"
assert_logs_exclude "$wrong_geometry" "$token_canary" 'authentication token contents'

viewer_disabled="${container_name}-viewer-disabled"
start_container "$viewer_disabled" --env VIEWER_ENABLED=0
wait_running_probe "$viewer_disabled"
docker exec "$viewer_disabled" sh -eu -c '
  test "$(pgrep -u 1000 -x s6-pause | wc -l)" -eq 2
  ! pgrep -u 1000 -x X0tigervnc >/dev/null
  ! pgrep -u 1000 -f "websockify --web=/usr/share/novnc" >/dev/null
  curl --fail --silent --show-error http://127.0.0.1:8080/readyz >/dev/null
'
assert_listener_absent "$viewer_disabled" 170C 'RFB'
assert_listener_absent "$viewer_disabled" 17C0 'WebSocket/noVNC'
"$repo_root/scripts/container/assert-idle-runtime.sh" "$viewer_disabled" bare 0
docker stop --time 35 "$viewer_disabled" >/dev/null
test "$(docker inspect "$viewer_disabled" --format '{{.State.ExitCode}}')" -eq 0
assert_logs_exclude "$viewer_disabled" "$token_canary" 'authentication token contents'

viewer_optional="${container_name}-viewer-optional-restart"
start_container "$viewer_optional"
wait_running_probe "$viewer_optional"
old_viewer_pid=$(docker exec "$viewer_optional" pgrep -xo X0tigervnc)
for failure in 1 2 3 4; do
  kill_service_payload "$viewer_optional" x0tigervnc
  new_viewer_pid=
  for _ in {1..45}; do
    new_viewer_pid=$(docker exec "$viewer_optional" pgrep -xo X0tigervnc 2>/dev/null || true)
    if [[ -n $new_viewer_pid && $new_viewer_pid != "$old_viewer_pid" ]] \
      && docker exec "$viewer_optional" /usr/local/libexec/xenoteer/healthcheck \
        >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
  test -n "$new_viewer_pid"
  test "$new_viewer_pid" != "$old_viewer_pid"
  old_viewer_pid=$new_viewer_pid
  expected_delay=$((1 << (failure - 1)))
  assert_logs_contain "$viewer_optional" \
    "optional viewer service x0tigervnc exited unexpectedly; retrying after $expected_delay seconds" \
    "optional viewer retry $failure with delay $expected_delay"
done

# The fifth close failure reaches the bounded ceiling. The optional viewer is
# allowed to stay down, but daemon readiness must become truthfully Degraded.
kill_service_payload "$viewer_optional" x0tigervnc
for _ in {1..30}; do
  if ! docker exec "$viewer_optional" pgrep -xo X0tigervnc >/dev/null 2>&1 \
    && docker exec "$viewer_optional" awk \
      '$2 >= 5 && $3 == "degraded" { found=1 } END { exit found ? 0 : 1 }' \
      /run/xenoteer/viewer/x0tigervnc-restarts 2>/dev/null; then
    break
  fi
  sleep 1
done
if docker exec "$viewer_optional" pgrep -xo X0tigervnc >/dev/null 2>&1; then
  printf 'optional viewer restarted before explicit recovery from its retry ceiling\n' >&2
  exit 1
fi
docker exec "$viewer_optional" awk \
  '$2 >= 5 && $3 == "degraded" { found=1 } END { exit found ? 0 : 1 }' \
  /run/xenoteer/viewer/x0tigervnc-restarts
degraded_observed=0
for _ in {1..30}; do
  ready_body=$(docker exec "$viewer_optional" curl --fail --silent --show-error \
    http://127.0.0.1:8080/readyz 2>/dev/null || true)
  if [[ $ready_body == '{"status":"degraded"}' ]] \
    && docker exec "$viewer_optional" /usr/local/libexec/xenoteer/healthcheck \
      >/dev/null 2>&1; then
    degraded_observed=1
    break
  fi
  sleep 1
done
test "$degraded_observed" -eq 1
test "$(docker inspect "$viewer_optional" --format '{{.State.Running}}')" = true
docker exec "$viewer_optional" curl --fail --silent --show-error \
  http://127.0.0.1:8080/livez >/dev/null
assert_logs_contain "$viewer_optional" \
  'optional viewer service x0tigervnc reached its restart ceiling; waiting for explicit operator recovery' \
  'the optional viewer restart-ceiling diagnostic'

# Explicit operator recovery resets the bounded restart budget before bringing
# the optional service up. Monitoring must restore Ready without replacing core
# processes, and the next crash must consume retry 1 rather than retry 6.
docker exec "$viewer_optional" rm -f /run/xenoteer/viewer/x0tigervnc-restarts
docker exec "$viewer_optional" test ! -e /run/xenoteer/viewer/x0tigervnc-restarts
docker exec "$viewer_optional" /command/s6-svc -u /run/service/x0tigervnc
recovered_viewer_pid=
for _ in {1..60}; do
  recovered_viewer_pid=$(docker exec "$viewer_optional" pgrep -xo X0tigervnc 2>/dev/null || true)
  if [[ -n $recovered_viewer_pid && $recovered_viewer_pid != "$old_viewer_pid" ]] \
    && docker exec "$viewer_optional" /usr/local/libexec/xenoteer/probe-viewer-protocol \
      >/dev/null 2>&1 \
    && docker exec "$viewer_optional" /usr/local/libexec/xenoteer/healthcheck \
      >/dev/null 2>&1 \
    && [[ $(docker exec "$viewer_optional" curl --fail --silent --show-error \
      http://127.0.0.1:8080/readyz 2>/dev/null || true) == '{"status":"ready"}' ]]; then
    break
  fi
  sleep 1
done
test -n "$recovered_viewer_pid"
test "$recovered_viewer_pid" != "$old_viewer_pid"
test "$(docker exec "$viewer_optional" curl --fail --silent --show-error \
  http://127.0.0.1:8080/readyz)" = '{"status":"ready"}'

kill_service_payload "$viewer_optional" x0tigervnc
post_recovery_pid=
for _ in {1..45}; do
  post_recovery_pid=$(docker exec "$viewer_optional" pgrep -xo X0tigervnc 2>/dev/null || true)
  if [[ -n $post_recovery_pid && $post_recovery_pid != "$recovered_viewer_pid" ]] \
    && docker exec "$viewer_optional" /usr/local/libexec/xenoteer/healthcheck \
      >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
test -n "$post_recovery_pid"
test "$post_recovery_pid" != "$recovered_viewer_pid"
docker exec "$viewer_optional" awk \
  '$2 == 1 && $3 == "retrying" { found=1 } END { exit found ? 0 : 1 }' \
  /run/xenoteer/viewer/x0tigervnc-restarts
if logs_contain "$viewer_optional" 'critical service x0tigervnc'; then
  printf 'optional X0tigervnc exit was classified as critical\n' >&2
  exit 1
fi
docker stop --time 35 "$viewer_optional" >/dev/null
test "$(docker inspect "$viewer_optional" --format '{{.State.ExitCode}}')" -eq 0

for critical in xvfb xenoteer-processd xenoteerd session-dbus atspi xfce; do
  name="${container_name}-${critical}"
  start_container "$name"
  wait_running_probe "$name"
  kill_service_payload "$name" "$critical"
  wait_stopped "$name"
  test "$(docker inspect "$name" --format '{{.State.ExitCode}}')" -ne 0
  # An upstream X11 or D-Bus death can synchronously kill a downstream service
  # before the deliberately signalled supervisor reaches its finish hook. The
  # atomic claimant must therefore be unique and causally downstream, not
  # necessarily the process that the harness signalled first.
  assert_critical_shutdown "$name" "$critical" normal
  assert_logs_exclude "$name" "$token_canary" 'authentication token contents'
done

for critical in xvfb xenoteer-processd xenoteerd session-dbus atspi xfce; do
  name="${container_name}-hardened-${critical}"
  start_container "$name" "${hardened_args[@]}"
  wait_running_probe "$name"
  kill_service_payload "$name" "$critical"
  wait_stopped "$name"
  test "$(docker inspect "$name" --format '{{.State.ExitCode}}')" -ne 0
  assert_critical_shutdown "$name" "$critical" hardened
  assert_logs_exclude "$name" "$token_canary" 'authentication token contents'
done

for critical in x0tigervnc websockify; do
  name="${container_name}-viewer-required-${critical}"
  start_container "$name" --env VIEWER_REQUIRED=1
  wait_running_probe "$name"
  kill_service_payload "$name" "$critical"
  wait_stopped "$name"
  test "$(docker inspect "$name" --format '{{.State.ExitCode}}')" -ne 0
  assert_critical_shutdown "$name" "$critical" viewer-required
  assert_logs_exclude "$name" "$token_canary" 'authentication token contents'
done

viewer_invalid="${container_name}-viewer-required-disabled"
start_container "$viewer_invalid" --env VIEWER_REQUIRED=1 --env VIEWER_ENABLED=0
wait_stopped "$viewer_invalid"
test "$(docker inspect "$viewer_invalid" --format '{{.State.ExitCode}}')" -ne 0
assert_logs_contain "$viewer_invalid" 'a required viewer cannot be disabled' \
  'the required-viewer configuration diagnostic'
assert_logs_exclude "$viewer_invalid" "$token_canary" 'authentication token contents'

no_secret="${container_name}-no-secret"
docker run --detach --name "$no_secret" --cpus 2 --shm-size=4g "$image" >/dev/null
created+=("$no_secret")
wait_running_probe "$no_secret"
docker exec "$no_secret" sh -eu -c '
  token=/run/xenoteer/generated-api-token
  test "$(stat -c %a:%u:%g "$token")" = 400:0:0
  test "$(wc -c <"$token")" -eq 64
  grep -Eq "^[0-9a-f]{64}$" "$token"
  {
    printf "header = \"Authorization: Bearer "
    cat "$token"
    printf "\"\n"
  } | curl --fail --silent --show-error --config - \
    http://127.0.0.1:8080/v1/status >/dev/null
'
generated_token=$(docker exec "$no_secret" cat /run/xenoteer/generated-api-token)
assert_logs_contain "$no_secret" \
  'generated API bearer token is available to root at /run/xenoteer/generated-api-token' \
  'the generated-token retrieval diagnostic'
assert_logs_exclude "$no_secret" "$generated_token" \
  'generated authentication token contents'
docker stop --time 35 "$no_secret" >/dev/null
test "$(docker inspect "$no_secret" --format '{{.State.ExitCode}}')" -eq 0

printf 'container image tests passed: %s\n' "$image"
