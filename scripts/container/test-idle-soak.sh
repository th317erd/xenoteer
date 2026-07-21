#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
image=${1:-${XENOTEER_IMAGE:-xenoteer:dev}}
duration=${XENOTEER_IDLE_SOAK_SECONDS:-1800}
interval=${XENOTEER_IDLE_SOAK_INTERVAL_SECONDS:-30}
profile=${DESKTOP_PROFILE:-bare}
viewer_enabled=${VIEWER_ENABLED:-1}
hardened=${XENOTEER_IDLE_SOAK_HARDENED:-0}
case "$duration:$interval" in *[!0-9:]*|0:*|*:0) exit 64 ;; esac
case "$profile" in bare|standard) ;; *) exit 64 ;; esac
case "$viewer_enabled:$hardened" in [01]:[01]) ;; *) exit 64 ;; esac
if (( interval > 60 )); then
  printf 'idle soak interval must be at most 60 seconds\n' >&2
  exit 64
fi

name="xenoteer-idle-soak-$$"
token_file=$(mktemp /tmp/xenoteer-idle-soak-token.XXXXXX)
cleanup() {
  docker rm --force --volumes "$name" >/dev/null 2>&1 || true
  rm -f -- "$token_file"
}
trap cleanup EXIT INT TERM

openssl rand -out "$token_file" 32
chmod 0400 "$token_file"
if [[ $(id -u) -eq 0 ]]; then
  chown 1000:1000 "$token_file"
elif [[ $(id -u) -ne 1000 ]]; then
  printf 'idle soak must run as root or UID 1000\n' >&2
  exit 77
fi

runtime_args=(
  --detach
  --name "$name"
  --env "DESKTOP_PROFILE=$profile"
  --env "VIEWER_ENABLED=$viewer_enabled"
  --cpus 2
  --shm-size 4g
  --security-opt "seccomp=$repo_root/container/spikes/browser/seccomp_profile.json"
  --volume "$token_file:/run/secrets/xenoteer_api_token:ro"
)
if [[ $hardened == 1 ]]; then
  runtime_args+=(
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
    --tmpfs '/run:rw,nosuid,nodev,exec,size=512m,mode=0755'
    --tmpfs '/tmp:rw,nosuid,nodev,noexec,size=1g,mode=1777'
    --volume /home/xenoteer
    --volume /workspace
  )
fi
docker run "${runtime_args[@]}" "$image" >/dev/null

for _ in {1..180}; do
  if docker exec "$name" /usr/local/libexec/xenoteer/healthcheck >/dev/null 2>&1; then
    break
  fi
  if [[ $(docker inspect "$name" --format '{{.State.Running}}') != true ]]; then
    docker logs "$name" >&2
    exit 1
  fi
  sleep 0.5
done
docker exec "$name" /usr/local/libexec/xenoteer/healthcheck >/dev/null

service_identities() {
  docker exec --interactive "$name" python3 - "$profile" "$viewer_enabled" <<'PY'
from __future__ import annotations

import pathlib
import sys

profile, viewer_enabled = sys.argv[1:]


def read_processes() -> dict[int, dict[str, object]]:
    processes: dict[int, dict[str, object]] = {}
    for path in pathlib.Path("/proc").glob("[0-9]*"):
        try:
            pid = int(path.name)
            status = (path / "status").read_text()
            uid = int(
                next(line for line in status.splitlines() if line.startswith("Uid:"))
                .split()[1]
            )
            if uid != 1000:
                continue
            comm = (path / "comm").read_text().strip()
            argv = tuple(
                argument.decode("utf-8", "strict")
                for argument in (path / "cmdline").read_bytes().rstrip(b"\0").split(b"\0")
            )
            stat_tail = (path / "stat").read_text().rsplit(")", 1)[1].split()
            starttime = int(stat_tail[19])
        except (FileNotFoundError, PermissionError, StopIteration, UnicodeDecodeError, ValueError):
            continue
        processes[pid] = {
            "pid": pid,
            "comm": comm,
            "argv": argv,
            "starttime": starttime,
        }
    return processes


def exact_argv(*expected: str):
    return lambda process: process["argv"] == expected


def atspi_dbus(process: dict[str, object]) -> bool:
    argv = process["argv"]
    return (
        isinstance(argv, tuple)
        and len(argv) == 6
        and argv[0] == "/usr/bin/dbus-daemon"
        and argv[1] == "--config-file=/usr/share/defaults/at-spi2/accessibility.conf"
        and argv[2] == "--nofork"
        and argv[3] == "--print-address"
        and argv[4].isdigit()
        and argv[5] == "--address=unix:path=/run/user/1000/at-spi/bus_99"
    )


expected = [
    (
        "xvfb",
        exact_argv(
            "Xvfb",
            ":99",
            "-screen",
            "0",
            "1920x1080x24",
            "-dpi",
            "96",
            "-noreset",
            "-nolisten",
            "tcp",
            "-auth",
            "/run/user/1000/Xauthority",
        ),
    ),
    (
        "session-dbus",
        exact_argv(
            "dbus-daemon",
            "--session",
            "--nofork",
            "--nopidfile",
            "--nosyslog",
            "--address=unix:path=/run/user/1000/bus",
        ),
    ),
    (
        "atspi-bus-launcher",
        exact_argv(
            "/usr/libexec/at-spi-bus-launcher",
            "--launch-immediately",
            "--a11y=1",
            "--screen-reader=1",
        ),
    ),
    ("atspi-dbus", atspi_dbus),
    (
        "atspi-registry",
        exact_argv("/usr/libexec/at-spi2-registryd", "--use-gnome-session"),
    ),
    ("xfce-session", exact_argv("xfce4-session", "--disable-tcp")),
    (
        "xfconf-daemon",
        exact_argv("/usr/lib/x86_64-linux-gnu/xfce4/xfconf/xfconfd"),
    ),
    ("xfwm", exact_argv("xfwm4", "--compositor=off")),
    ("xfsettings", exact_argv("xfsettingsd")),
    ("dconf", exact_argv("/usr/libexec/dconf-service")),
    ("xfdesktop", exact_argv("xfdesktop")),
    ("xenoteerd", exact_argv("/usr/local/bin/xenoteerd")),
]
if profile == "standard":
    expected.append(("xfce-panel", exact_argv("xfce4-panel")))
if viewer_enabled == "1":
    expected.extend(
        [
            (
                "x0tigervnc",
                exact_argv(
                    "X0tigervnc",
                    "-display",
                    ":99",
                    "-rfbport",
                    "5900",
                    "-interface",
                    "127.0.0.1",
                    "-localhost=1",
                    "-UseIPv4=1",
                    "-UseIPv6=0",
                    "-SecurityTypes=None",
                    "-AlwaysShared=1",
                    "-DisconnectClients=0",
                    "-AcceptKeyEvents=0",
                    "-AcceptPointerEvents=0",
                    "-AcceptSetDesktopSize=0",
                    "-AcceptCutText=0",
                    "-SendCutText=0",
                    "-SetPrimary=0",
                    "-SendPrimary=0",
                    "-MaxCutText=1024",
                ),
            ),
            (
                "websockify",
                exact_argv(
                    "/usr/bin/python3",
                    "/usr/bin/websockify",
                    "--web=/usr/share/novnc",
                    "--heartbeat=30",
                    "127.0.0.1:6080",
                    "127.0.0.1:5900",
                ),
            ),
        ]
    )

remaining = read_processes()
identities: list[tuple[str, int, int]] = []
for label, predicate in expected:
    matches = [process for process in remaining.values() if predicate(process)]
    if len(matches) != 1:
        raise SystemExit(
            f"persistent process {label!r} matched {len(matches)}, expected exactly one"
        )
    process = matches[0]
    pid = int(process["pid"])
    identities.append((label, pid, int(process["starttime"])))
    del remaining[pid]

if viewer_enabled == "0":
    pauses = sorted(
        (
            process
            for process in remaining.values()
            if process["argv"] == ("s6-pause",)
        ),
        key=lambda process: int(process["pid"]),
    )
    if len(pauses) != 2:
        raise SystemExit(
            f"disabled viewer pause processes matched {len(pauses)}, expected exactly two"
        )
    for index, process in enumerate(pauses, start=1):
        pid = int(process["pid"])
        identities.append((f"viewer-pause-{index}", pid, int(process["starttime"])))
        del remaining[pid]

if remaining:
    details = [
        f"pid={pid},comm={process['comm']!r},argv={process['argv']!r}"
        for pid, process in sorted(remaining.items())
    ]
    raise SystemExit("unexpected persistent UID-1000 process identities: " + "; ".join(details))

for label, pid, starttime in sorted(identities):
    print(f"{label}\t{pid}\t{starttime}")
PY
}

# Docker runs the image HEALTHCHECK independently of this soak loop. Its
# short-lived UID-1000 probe-xfce process (and children such as pgrep) can
# legitimately overlap a /proc snapshot. Require a clean, exact identity graph
# within a bounded window instead of weakening the persistent-process
# allowlist. A real extra, missing, or restarted service remains present and
# still fails after the retry window.
stable_service_identities() {
  local output
  for _ in {1..100}; do
    if output=$(service_identities 2>&1); then
      printf '%s\n' "$output"
      return 0
    fi
    sleep 0.1
  done
  printf '%s\n' "$output" >&2
  return 1
}

baseline_identities=$(stable_service_identities)
started=$(date +%s)
deadline=$((started + duration))
samples=0
while (( $(date +%s) < deadline )); do
  "$repo_root/scripts/container/assert-idle-runtime.sh" \
    "$name" "$profile" "$viewer_enabled"
  current_identities=$(stable_service_identities)
  if [[ $current_identities != "$baseline_identities" ]]; then
    printf 'idle service (PID,/proc starttime) graph changed during soak\n' >&2
    diff -u <(printf '%s\n' "$baseline_identities") \
      <(printf '%s\n' "$current_identities") >&2 || true
    exit 1
  fi
  samples=$((samples + 1))
  remaining=$((deadline - $(date +%s)))
  (( remaining > 0 )) || break
  sleep_for=$interval
  (( remaining < sleep_for )) && sleep_for=$remaining
  sleep "$sleep_for"
done

"$repo_root/scripts/container/assert-idle-runtime.sh" \
  "$name" "$profile" "$viewer_enabled"
test "$(stable_service_identities)" = "$baseline_identities"
docker stop --time 35 "$name" >/dev/null
test "$(docker inspect "$name" --format '{{.State.ExitCode}}')" -eq 0
if docker logs "$name" 2>&1 | grep -Fq 'exited unexpectedly'; then
  printf 'idle soak ended with an unexpected service exit\n' >&2
  exit 1
fi
printf 'idle soak passed: %ss, %s samples, profile=%s, viewer=%s, hardened=%s\n' \
  "$duration" "$samples" "$profile" "$viewer_enabled" "$hardened"
