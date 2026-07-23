#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
image_ref=${1:-${XENOTEER_DESKTOP_APPS_IMAGE:-xenoteer:desktop-apps-test}}
image=$(docker image inspect "$image_ref" --format '{{.Id}}')
if [[ ! $image =~ ^sha256:[0-9a-f]{64}$ ]]; then
  printf 'fixture image did not resolve to an immutable image ID: %s\n' "$image_ref" >&2
  exit 1
fi
base_image_id=$(docker image inspect "$image" \
  --format '{{index .Config.Labels "com.aeor.xenoteer.fixture.base-image-id"}}')
if [[ ! $base_image_id =~ ^sha256:[0-9a-f]{64}$ ]]; then
  printf 'fixture image does not record an exact base image ID: %s\n' "$image" >&2
  exit 1
fi
docker image inspect "$base_image_id" >/dev/null
docker image inspect "$base_image_id" "$image" | python3 -c '
import json
import sys

base, fixture = json.load(sys.stdin)
base_layers = base["RootFS"]["Layers"]
fixture_layers = fixture["RootFS"]["Layers"]
if fixture_layers[: len(base_layers)] != base_layers:
    raise SystemExit("fixture image layer ancestry differs from its recorded exact base")
if base["Id"] != sys.argv[1] or fixture["Id"] != sys.argv[2]:
    raise SystemExit("fixture/base image identity changed while resolving matrix inputs")
' "$base_image_id" "$image"
matrix_scope=${XENOTEER_DESKTOP_MATRIX_SCOPE:-full}
case "$matrix_scope" in
  full|hardened-only) ;;
  *) printf 'XENOTEER_DESKTOP_MATRIX_SCOPE must be full or hardened-only\n' >&2; exit 64 ;;
esac
prefix="xenoteer-desktop-apps-$$"
token_file=$(mktemp /tmp/xenoteer-desktop-token.XXXXXX)
containers=()
volumes=(
  "${prefix}-clean-a-home"
  "${prefix}-clean-a-workspace"
  "${prefix}-clean-b-home"
  "${prefix}-clean-b-workspace"
  "${prefix}-persistent-home"
  "${prefix}-persistent-workspace"
  "${prefix}-hardened-home"
  "${prefix}-hardened-workspace"
)

cleanup() {
  local container
  for container in "${containers[@]}"; do
    docker rm --force --volumes "$container" >/dev/null 2>&1 || true
  done
  docker volume rm "${volumes[@]}" >/dev/null 2>&1 || true
  rm -f -- "$token_file"
}
trap cleanup EXIT INT TERM

printf '%064d' 0 >"$token_file"
chmod 0400 "$token_file"
if [[ $(id -u) -eq 0 ]]; then
  chown 0:0 "$token_file"
elif ! docker info --format '{{json .SecurityOptions}}' | grep -Fq 'name=rootless'; then
  printf 'desktop app image test must run as root or use rootless Docker for a container-root-owned token\n' >&2
  exit 77
fi
for volume in "${volumes[@]}"; do
  docker volume create "$volume" >/dev/null
done

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
  --cpus 2
  --tmpfs "/run:rw,nosuid,nodev,exec,size=512m,mode=0755"
  --tmpfs "/tmp:rw,nosuid,nodev,noexec,size=1g,mode=1777"
)

start_container() {
  local name=$1 profile=$2 mode=$3 home_volume=$4 workspace_volume=$5
  local -a mode_args=(--cpus 2)
  if [[ $mode == hardened ]]; then
    mode_args=("${hardened_args[@]}")
  fi
  docker run --detach \
    --name "$name" \
    --env "DESKTOP_PROFILE=$profile" \
    --network none \
    --shm-size 4g \
    --security-opt "seccomp=$repo_root/container/spikes/browser/seccomp_profile.json" \
    --volume "$token_file:/run/secrets/xenoteer_api_token:ro" \
    --volume "$home_volume:/home/xenoteer" \
    --volume "$workspace_volume:/workspace" \
    "${mode_args[@]}" \
    "$image" >/dev/null
  containers+=("$name")
}

stop_container() {
  local name=$1
  docker stop --time 35 "$name" >/dev/null
  docker rm "$name" >/dev/null
}

assert_persistent_home_isolated() {
  local name=$1
  docker exec "$name" grep -Fxq persistent-session-canary \
    /home/xenoteer/.cache/sessions/xfce4-session-resurrect
  docker exec "$name" test -f /home/xenoteer/.config/autostart/resurrect.desktop
  if docker exec "$name" pgrep -x sleep >/dev/null 2>&1; then
    printf 'persistent HOME autostart unexpectedly ran\n' >&2
    return 1
  fi
}

desktop_exec() {
  local name=$1
  shift
  docker exec --user 1000 "$name" \
    /command/s6-envdir -f -L /run/xenoteer/env "$@"
}

wait_desktop() {
  local name=$1
  for _ in {1..240}; do
    if desktop_exec "$name" wmctrl -m >/dev/null 2>&1 \
      && docker exec "$name" pgrep -x xfce4-session >/dev/null 2>&1 \
      && docker exec "$name" pgrep -x xfwm4 >/dev/null 2>&1 \
      && docker exec "$name" pgrep -x xfsettingsd >/dev/null 2>&1 \
      && docker exec "$name" pgrep -x xfdesktop >/dev/null 2>&1; then
      return 0
    fi
    if [[ $(docker inspect "$name" --format '{{.State.Running}}') != true ]]; then
      docker logs "$name" >&2
      return 1
    fi
    sleep 0.25
  done
  docker logs "$name" >&2
  printf 'desktop did not become ready: %s\n' "$name" >&2
  return 1
}

assert_desktop_profile() {
  local name=$1 profile=$2
  "$repo_root/scripts/container/assert-idle-runtime.sh" "$name" "$profile" 1
  desktop_exec "$name" test -w /workspace
  docker exec "$name" sh -eu -c '
    test "$(stat -c %a:%u:%g /usr/lib/chromium/chrome-sandbox)" = 4755:0:0
    test "$(cat /run/xenoteer/env/XDG_CONFIG_HOME)" = /run/user/1000/xdg/config
    test "$(cat /run/xenoteer/env/XDG_CACHE_HOME)" = /run/user/1000/xdg/cache
    test "$(cat /run/xenoteer/env/XDG_DATA_HOME)" = /run/user/1000/xdg/data
    test "$(stat -c %a:%u:%g /run/user/1000/xdg)" = 700:1000:1000
    test -z "$(find /run/user/1000/xdg/cache/sessions -mindepth 1 -print -quit)"
    test "$(pgrep -xc xfce4-session)" -eq 1
    test "$(pgrep -xc xfwm4)" -eq 1
    test "$(pgrep -xc xfsettingsd)" -eq 1
    test "$(pgrep -xc xfdesktop)" -eq 1
    ! pgrep -x Thunar >/dev/null
    # Linux exposes comm names at TASK_COMM_LEN (16 bytes including NUL).
    ! pgrep -x xfce4-power-man >/dev/null
    ! pgrep -x xfce4-screensav >/dev/null
    ! pgrep -x light-locker >/dev/null
    ! pgrep -x xfce4-notifyd >/dev/null
    ! pgrep -x ssh-agent >/dev/null
    ! pgrep -x gpg-agent >/dev/null
  '
  if [[ $profile == bare ]]; then
    if docker exec "$name" pgrep -x xfce4-panel >/dev/null 2>&1; then
      printf 'bare profile unexpectedly started xfce4-panel\n' >&2
      return 1
    fi
  else
    test "$(docker exec "$name" pgrep -xc xfce4-panel)" -eq 1
  fi
  test "$(desktop_exec "$name" xfconf-query -c xfwm4 -p /general/use_compositing)" = false
  test "$(desktop_exec "$name" xfconf-query -c xfwm4 -p /general/workspace_count)" = 1
  desktop_exec "$name" xprop -root _NET_NUMBER_OF_DESKTOPS | grep -Eq '= 1$'
  docker exec "$name" sh -eu -c '
    xfwm_pid=$(pgrep -xo xfwm4)
    tr "\000" "\n" </proc/"$xfwm_pid"/cmdline | grep -Fxq -- --compositor=off
    ! pgrep -x picom >/dev/null
    ! pgrep -x compton >/dev/null
    ! pgrep -x xcompmgr >/dev/null
  '
  docker exec --interactive "$name" python3 - <<'PY'
import ipaddress
import pathlib
import socket

expected = {
    ("0.0.0.0", 8080),
    ("127.0.0.1", 5900),
    ("127.0.0.1", 6080),
}

observed = set()
for table in ("/proc/net/tcp", "/proc/net/tcp6"):
    for line in pathlib.Path(table).read_text().splitlines()[1:]:
        fields = line.split()
        if fields[3] != "0A":
            continue
        address, port_hex = fields[1].split(":")
        port = int(port_hex, 16)
        if table.endswith("tcp"):
            host = socket.inet_ntoa(bytes.fromhex(address)[::-1])
        else:
            packed = b"".join(
                bytes.fromhex(address[offset : offset + 8])[::-1]
                for offset in range(0, 32, 8)
            )
            host = socket.inet_ntop(socket.AF_INET6, packed)
        observed.add((str(ipaddress.ip_address(host)), port))
if observed != expected:
    raise SystemExit(
        f"TCP listener inventory differs: observed={sorted(observed)!r}, "
        f"expected={sorted(expected)!r}"
    )
PY
}

assert_no_internet() {
  local name=$1
  docker exec --interactive "$name" python3 - <<'PY'
import socket

loopback = socket.create_connection(("127.0.0.1", 6080), timeout=2)
loopback.close()
external = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
external.settimeout(1)
try:
    if external.connect_ex(("1.1.1.1", 443)) == 0:
        raise SystemExit("fixture network unexpectedly reached the public Internet")
finally:
    external.close()
PY
}

launch_detached() {
  local name=$1 log=$2
  shift 2
  docker exec --detach --user 1000 "$name" sh -c \
    'log=$1; shift; exec /command/s6-envdir -f -L /run/xenoteer/env "$@" >"$log" 2>&1' \
    sh "$log" "$@"
}

wait_window() {
  local name=$1 title=$2
  for _ in {1..120}; do
    if desktop_exec "$name" wmctrl -l | grep -Fq "$title"; then
      return 0
    fi
    sleep 0.25
  done
  docker exec "$name" sh -c 'for log in /run/user/1000/*.log; do test ! -f "$log" || { echo "$log"; tail -n 100 "$log"; }; done' >&2
  printf 'window did not appear: %s\n' "$title" >&2
  return 1
}

atspi_names() {
  local name=$1
  shift
  local -a arguments=()
  local expected
  for expected in "$@"; do
    arguments+=(--name "$expected")
  done
  desktop_exec "$name" \
    /usr/share/xenoteer/fixtures/desktop-apps/atspi-probe.py "${arguments[@]}"
}

atspi_absent() {
  local name=$1
  shift
  local -a arguments=()
  local forbidden
  for forbidden in "$@"; do
    arguments+=(--absent-name "$forbidden")
  done
  desktop_exec "$name" \
    /usr/share/xenoteer/fixtures/desktop-apps/atspi-probe.py "${arguments[@]}"
}

assert_window_metadata() {
  local name=$1 main_title=$2 dialog_title=$3 expected_class=$4 dialog_id
  desktop_exec "$name" wmctrl -lx | grep -F "$main_title" | grep -Fiq "$expected_class"
  dialog_id=$(desktop_exec "$name" wmctrl -l \
    | awk -v title="$dialog_title" 'index($0, title) { print $1; exit }')
  test -n "$dialog_id"
  desktop_exec "$name" xprop -id "$dialog_id" WM_TRANSIENT_FOR \
    | grep -Eq 'window id # 0x[1-9a-fA-F][0-9a-fA-F]*$'
}

stop_fixture() {
  local name=$1 pattern=$2
  docker exec "$name" pkill -TERM -f "$pattern" >/dev/null 2>&1 || true
  for _ in {1..80}; do
    if ! docker exec "$name" pgrep -f "$pattern" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  docker exec "$name" pkill -KILL -f "$pattern" >/dev/null 2>&1 || true
  sleep 0.25
  if docker exec "$name" pgrep -f "$pattern" >/dev/null 2>&1; then
    printf 'fixture survived SIGKILL: %s\n' "$pattern" >&2
  else
    printf 'fixture failed to stop cleanly and required SIGKILL: %s\n' "$pattern" >&2
  fi
  return 1
}

assert_chromium_sandbox() {
  local name=$1 browser=${2:-chromium}
  local profile=/run/user/1000/xdg/data/xenoteer/browser-profiles/$browser
  for _ in {1..120}; do
    if desktop_exec "$name" test -s "$profile/DevToolsActivePort"; then
      local port
      port=$(desktop_exec "$name" sed -n '1p' "$profile/DevToolsActivePort")
      if [[ $port =~ ^[0-9]+$ ]]; then
        desktop_exec "$name" \
          /usr/share/xenoteer/fixtures/desktop-apps/chromium-sandbox-probe.py "$port"
        return 0
      fi
    fi
    sleep 0.25
  done
  printf '%s DevToolsActivePort did not become ready\n' "$browser" >&2
  return 1
}

assert_electron_sandbox() {
  local name=$1
  local profile=/run/user/1000/xdg/data/xenoteer/browser-profiles/electron
  local probe=$repo_root/container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/electron-sandbox-probe.py
  for _ in {1..120}; do
    if desktop_exec "$name" test -s "$profile/DevToolsActivePort"; then
      local port
      port=$(desktop_exec "$name" sed -n '1p' "$profile/DevToolsActivePort")
      if [[ $port =~ ^[0-9]+$ ]]; then
        docker exec --interactive --user 1000 "$name" \
          /command/s6-envdir -f -L /run/xenoteer/env python3 - "$port" <"$probe"
        return 0
      fi
    fi
    sleep 0.25
  done
  printf 'Electron DevToolsActivePort did not become ready\n' >&2
  return 1
}

audit_browser_processes() {
  local name=$1 kind=$2 mode=$3
  docker exec --interactive "$name" python3 - "$kind" "$mode" <<'PY'
import json
import pathlib
import sys

kind = sys.argv[1]
mode = sys.argv[2]
matched = 0
renderers = 0
nested_pid_namespace = False
for command_path in pathlib.Path("/proc").glob("[0-9]*/cmdline"):
    try:
        raw = command_path.read_bytes()
        argv = [part.decode(errors="replace") for part in raw.split(b"\0") if part]
        if not argv:
            continue
        # QtWebEngine rewrites cmdline to one space-delimited field, unlike a
        # conventional NUL-delimited argv. Classify only its leading executable
        # path so the exact binary remains auditable on both representations.
        argv0 = argv[0].split(" ", 1)[0]
        executable = pathlib.Path(argv0).name
        command = " ".join(argv)
        status = command_path.with_name("status").read_text()
    except (FileNotFoundError, PermissionError):
        continue
    if kind == "chromium" and not executable.startswith("chromium"):
        continue
    if kind == "electron" and executable != "electron":
        continue
    if kind == "qtwebengine" and executable != "QtWebEngineProcess":
        continue
    if kind == "firefox" and executable not in {"firefox", "firefox-esr", "firefox-bin"}:
        continue
    matched += 1
    if "--no-sandbox" in command or "--disable-dev-shm-usage" in command:
        raise SystemExit(f"forbidden browser flag: {command}")
    uid_line = next(line for line in status.splitlines() if line.startswith("Uid:"))
    if int(uid_line.split()[1]) != 1000:
        raise SystemExit(f"browser process is not UID 1000: {command_path}")
    if mode == "hardened":
        capabilities = {
            field: int(
                next(item for item in status.splitlines() if item.startswith(f"{field}:"))
                .split()[1],
                16,
            )
            for field in ("CapInh", "CapPrm", "CapEff", "CapAmb")
        }
        if capabilities["CapInh"] != 0 or capabilities["CapAmb"] != 0:
            raise SystemExit(
                f"browser process retained inherited/ambient capability: {capabilities!r}; "
                f"{command_path}; command={command}"
            )
        if capabilities["CapPrm"] != 0 or capabilities["CapEff"] != 0:
            cap_sys_admin = 1 << 21
            nspid = next(line for line in status.splitlines() if line.startswith("NSpid:"))
            uid_map = command_path.with_name("uid_map").read_text()
            initial_uid_map = pathlib.Path("/proc/1/uid_map").read_text()
            rows = [tuple(int(value) for value in line.split()) for line in uid_map.splitlines()]
            sandbox_zygote = (
                "--type=zygote" in command
                and capabilities["CapPrm"] == cap_sys_admin
                and capabilities["CapEff"] == cap_sys_admin
                and len(nspid.split()) >= 3
                and uid_map != initial_uid_map
                and sum(length for _inside, _outside, length in rows) < 4_294_967_295
            )
            if not sandbox_zygote:
                raise SystemExit(
                    f"non-zygote browser process retained permitted/effective capability: "
                    f"{capabilities!r}; uid_map={uid_map!r}; {command_path}; command={command}"
                )
            print(
                json.dumps(
                    {
                        "type": "browser_userns_zygote_capability",
                        "kind": kind,
                        "capability": "CAP_SYS_ADMIN",
                        "pid": int(command_path.parent.name),
                        "uid_map": [list(row) for row in rows],
                    },
                    sort_keys=True,
                ),
                flush=True,
            )
    if "--type=renderer" in command or "QtWebEngineProcess" in command or "-contentproc" in command:
        renderers += 1
        seccomp = next(line for line in status.splitlines() if line.startswith("Seccomp:"))
        if seccomp.split()[1] != "2":
            raise SystemExit(f"browser subprocess lacks seccomp mode 2: {command_path}")
        no_new_privileges = next(
            line for line in status.splitlines() if line.startswith("NoNewPrivs:")
        )
        if no_new_privileges.split()[1] != "1":
            raise SystemExit(f"browser subprocess lacks no-new-privileges: {command_path}")
        nspid = next(line for line in status.splitlines() if line.startswith("NSpid:"))
        if len(nspid.split()) >= 3:
            nested_pid_namespace = True
if matched == 0 or renderers == 0:
    raise SystemExit(f"no auditable {kind} browser/subprocess tree")
if kind == "qtwebengine" and not nested_pid_namespace:
    raise SystemExit("QtWebEngine created no subprocess in a nested PID namespace")
if kind == "electron" and not nested_pid_namespace:
    raise SystemExit("Electron created no subprocess in a nested PID namespace")
PY
}

audit_application_caps() {
  local name=$1 fixture=$2 mode=$3
  [[ $mode == hardened ]] || return 0
  docker exec --interactive "$name" python3 - "$fixture" <<'PY'
import os
import pathlib
import sys

fixture = sys.argv[1]
matched = 0
for command_path in pathlib.Path("/proc").glob("[0-9]*/cmdline"):
    if command_path.parent.name == str(os.getpid()):
        continue
    try:
        argv = [part.decode(errors="replace") for part in command_path.read_bytes().split(b"\0") if part]
        status = command_path.with_name("status").read_text()
    except (FileNotFoundError, PermissionError):
        continue
    if fixture not in argv:
        continue
    matched += 1
    uid = next(line for line in status.splitlines() if line.startswith("Uid:"))
    if int(uid.split()[1]) != 1000:
        raise SystemExit(f"desktop fixture is not UID 1000: {command_path}")
    for capability_field in ("CapInh:", "CapEff:", "CapPrm:", "CapAmb:"):
        line = next(item for item in status.splitlines() if item.startswith(capability_field))
        if int(line.split()[1], 16) != 0:
            raise SystemExit(
                f"desktop fixture retained {capability_field[:-1]}: {command_path}"
            )
if matched != 1:
    raise SystemExit(f"expected one desktop fixture owner for {fixture}, observed {matched}")
PY
}

audit_browser_absent() {
  local name=$1 kind=$2
  docker exec --interactive "$name" python3 - "$kind" <<'PY'
import pathlib
import sys
import time

kind = sys.argv[1]


def matching() -> list[str]:
    found = []
    for command_path in pathlib.Path("/proc").glob("[0-9]*/cmdline"):
        try:
            raw = command_path.read_bytes()
        except (FileNotFoundError, PermissionError):
            continue
        argv = [part.decode(errors="replace") for part in raw.split(b"\0") if part]
        if not argv:
            continue
        executable = pathlib.Path(argv[0].split(" ", 1)[0]).name
        if kind == "chromium" and executable.startswith("chromium"):
            found.append(str(command_path.parent))
        elif kind == "electron" and executable == "electron":
            found.append(str(command_path.parent))
        elif kind == "firefox" and executable in {"firefox", "firefox-esr", "firefox-bin"}:
            found.append(str(command_path.parent))
        elif kind == "qtwebengine" and executable == "QtWebEngineProcess":
            found.append(str(command_path.parent))
    return found


deadline = time.monotonic() + 15
while time.monotonic() < deadline:
    remaining = matching()
    if not remaining:
        raise SystemExit(0)
    time.sleep(0.25)
raise SystemExit(f"orphaned {kind} process tree after fixture teardown: {remaining!r}")
PY
}

start_shm_pressure() {
  local name=$1
  local fixture=/usr/share/xenoteer/fixtures/desktop-apps/shm-pressure.py
  local ready=/run/user/1000/shm-pressure-ready.json
  local before after consumed
  desktop_exec "$name" rm -f "$ready"
  before=$(desktop_exec "$name" df --output=avail -B1 /dev/shm | tail -n 1 | tr -d ' ')
  launch_detached "$name" /run/user/1000/shm-pressure.log \
    "$fixture" --bytes 536870912 --ready-file "$ready"
  for _ in {1..240}; do
    if desktop_exec "$name" test -s "$ready"; then
      after=$(desktop_exec "$name" df --output=avail -B1 /dev/shm | tail -n 1 | tr -d ' ')
      consumed=$((before - after))
      if ((consumed < 503316480)); then
        printf 'shared-memory pressure consumed only %s bytes\n' "$consumed" >&2
        return 1
      fi
      desktop_exec "$name" python3 - "$ready" <<'PY'
import json
import pathlib
import sys

payload = json.loads(pathlib.Path(sys.argv[1]).read_text())
if payload.get("type") != "shm_pressure_ready" or payload.get("bytes") != 536870912:
    raise SystemExit(f"unexpected shm pressure readiness: {payload!r}")
PY
      return 0
    fi
    sleep 0.1
  done
  printf 'shared-memory pressure fixture did not become ready\n' >&2
  return 1
}

stop_shm_pressure() {
  local name=$1
  local fixture=/usr/share/xenoteer/fixtures/desktop-apps/shm-pressure.py
  stop_fixture "$name" "$fixture"
  # The command substitution is intentionally evaluated by the container shell.
  # shellcheck disable=SC2016
  desktop_exec "$name" sh -eu -c \
    'test -z "$(find /dev/shm -maxdepth 1 -name "xenoteer-fixture-pressure-*" -print -quit)"'
}

assert_fixture_idle() {
  local name=$1 profile=$2
  "$repo_root/scripts/container/assert-idle-runtime.sh" "$name" "$profile" 1
}

run_application_matrix() {
  local name=$1 profile=$2 mode=$3
  local fixtures=/usr/share/xenoteer/fixtures/desktop-apps

  desktop_exec "$name" "$fixtures/browser-runtime-doctor" >/dev/null
  assert_no_internet "$name"

  launch_detached "$name" /run/user/1000/gtk3.log "$fixtures/gtk3-fixture.py"
  wait_window "$name" 'Xenoteer GTK3 Fixture — Main'
  wait_window "$name" 'Xenoteer GTK3 Fixture — Dialog'
  assert_window_metadata "$name" 'Xenoteer GTK3 Fixture — Main' \
    'Xenoteer GTK3 Fixture — Dialog' 'XenoteerFixture'
  atspi_names "$name" 'Xenoteer GTK3 Main Window' 'Stable Button' 'Stable Entry' \
    'Protected Entry' 'Stable Radio Alpha' 'Stable Slider' 'Stable Text Area' \
    'Stable Tabs' 'Stable Virtual List' 'Stable Custom Area' 'Disabled Button'
  audit_application_caps "$name" "$fixtures/gtk3-fixture.py" "$mode"
  stop_fixture "$name" "$fixtures/gtk3-fixture.py"
  atspi_absent "$name" 'Xenoteer GTK3 Main Window'
  assert_fixture_idle "$name" "$profile"

  launch_detached "$name" /run/user/1000/qt6.log "$fixtures/qt6-fixture.py"
  wait_window "$name" 'Xenoteer Qt6 Fixture — Main'
  wait_window "$name" 'Xenoteer Qt6 Fixture — Dialog'
  assert_window_metadata "$name" 'Xenoteer Qt6 Fixture — Main' \
    'Xenoteer Qt6 Fixture — Dialog' 'xenoteer-qt6-fixture'
  atspi_names "$name" 'Xenoteer Qt6 Main Window' 'Stable Button' 'Stable Entry' \
    'Protected Entry' 'Stable Radio Alpha' 'Stable Slider' 'Stable Text Area' \
    'Stable Tabs' 'Stable Virtual List' 'Stable Custom Area' 'Disabled Button'
  audit_application_caps "$name" "$fixtures/qt6-fixture.py" "$mode"
  stop_fixture "$name" "$fixtures/qt6-fixture.py"
  atspi_absent "$name" 'Xenoteer Qt6 Main Window'
  assert_fixture_idle "$name" "$profile"

  launch_detached "$name" /run/user/1000/chromium.log "$fixtures/launch-chromium-fixture"
  wait_window "$name" 'Xenoteer Chromium Browser Fixture'
  atspi_names "$name" 'Xenoteer Chromium Browser Fixture' 'Chromium fixture marker' \
    'Browser Stable Button' 'Stable entry' 'Protected entry' 'Editable content' \
    'Virtual tree' 'Stable iframe' 'Deterministic canvas'
  start_shm_pressure "$name"
  assert_chromium_sandbox "$name"
  audit_browser_processes "$name" chromium "$mode"
  stop_fixture "$name" chromium
  stop_shm_pressure "$name"
  atspi_absent "$name" 'Chromium fixture marker'
  audit_browser_absent "$name" chromium
  assert_fixture_idle "$name" "$profile"

  launch_detached "$name" /run/user/1000/firefox.log "$fixtures/launch-firefox-fixture"
  wait_window "$name" 'Xenoteer Firefox Browser Fixture'
  atspi_names "$name" 'Xenoteer Firefox Browser Fixture' 'Firefox fixture marker' \
    'Browser Stable Button' 'Stable entry' 'Protected entry' 'Editable content' \
    'Virtual tree' 'Stable iframe' 'Deterministic canvas'
  start_shm_pressure "$name"
  audit_browser_processes "$name" firefox "$mode"
  stop_fixture "$name" firefox
  stop_shm_pressure "$name"
  atspi_absent "$name" 'Firefox fixture marker'
  audit_browser_absent "$name" firefox
  assert_fixture_idle "$name" "$profile"

  launch_detached "$name" /run/user/1000/qtwebengine.log "$fixtures/launch-qtwebengine-fixture"
  wait_window "$name" 'Xenoteer QtWebEngine Browser Fixture'
  atspi_names "$name" 'Xenoteer QtWebEngine Main Window' 'QtWebEngine fixture marker' \
    'Browser Stable Button' 'Stable entry' 'Protected entry' 'Editable content' \
    'Virtual tree' 'Stable iframe' 'Deterministic canvas'
  start_shm_pressure "$name"
  audit_application_caps "$name" "$fixtures/qtwebengine-fixture.py" "$mode"
  audit_browser_processes "$name" qtwebengine "$mode"
  stop_fixture "$name" "$fixtures/qtwebengine-fixture.py"
  stop_shm_pressure "$name"
  atspi_absent "$name" 'QtWebEngine fixture marker'
  audit_browser_absent "$name" qtwebengine
  assert_fixture_idle "$name" "$profile"

  launch_detached "$name" /run/user/1000/electron.log "$fixtures/launch-electron-fixture"
  wait_window "$name" 'Xenoteer Electron Browser Fixture'
  atspi_names "$name" 'Xenoteer Electron Browser Fixture' 'Electron fixture marker' \
    'Browser Stable Button' 'Stable entry' 'Protected entry' 'Editable content' \
    'Virtual tree' 'Stable iframe' 'Deterministic canvas'
  start_shm_pressure "$name"
  assert_electron_sandbox "$name"
  audit_browser_processes "$name" electron "$mode"
  stop_fixture "$name" electron-main.js
  stop_shm_pressure "$name"
  atspi_absent "$name" 'Electron fixture marker'
  audit_browser_absent "$name" electron
  assert_fixture_idle "$name" "$profile"
}

if [[ $matrix_scope == full ]]; then
clean_a="${prefix}-clean-a"
start_container "$clean_a" bare normal \
  "${prefix}-clean-a-home" "${prefix}-clean-a-workspace"
wait_desktop "$clean_a"
assert_desktop_profile "$clean_a" bare
docker exec "$clean_a" test ! -e /home/xenoteer/.cache/sessions/xfce4-session-resurrect
docker exec "$clean_a" test ! -e /home/xenoteer/.config/autostart/resurrect.desktop
run_application_matrix "$clean_a" bare normal
stop_container "$clean_a"

clean_b="${prefix}-clean-b"
start_container "$clean_b" standard normal \
  "${prefix}-clean-b-home" "${prefix}-clean-b-workspace"
wait_desktop "$clean_b"
assert_desktop_profile "$clean_b" standard
docker exec "$clean_b" test ! -e /home/xenoteer/.cache/sessions/xfce4-session-resurrect
docker exec "$clean_b" test ! -e /home/xenoteer/.config/autostart/resurrect.desktop
stop_container "$clean_b"

# Persistent HOME contents remain byte-for-byte present but cannot participate
# in the session because every boot points XDG config/cache/data into /run.
docker run --rm --network none \
  --volume "${prefix}-persistent-home:/home/xenoteer" \
  --entrypoint sh "$image" -eu -c '
  install -d -m 0700 -o 1000 -g 1000 /home/xenoteer/.cache/sessions /home/xenoteer/.config/autostart
  printf "%s\n" persistent-session-canary >/home/xenoteer/.cache/sessions/xfce4-session-resurrect
  cat >/home/xenoteer/.config/autostart/resurrect.desktop <<EOF
[Desktop Entry]
Type=Application
Name=Forbidden persistent autostart
Exec=sleep 9999
EOF
  chown -R 1000:1000 /home/xenoteer/.cache /home/xenoteer/.config
'

persistent_a="${prefix}-persistent-a"
start_container "$persistent_a" bare normal \
  "${prefix}-persistent-home" "${prefix}-persistent-workspace"
wait_desktop "$persistent_a"
assert_desktop_profile "$persistent_a" bare
assert_persistent_home_isolated "$persistent_a"
stop_container "$persistent_a"

persistent_b="${prefix}-persistent-b"
start_container "$persistent_b" bare normal \
  "${prefix}-persistent-home" "${prefix}-persistent-workspace"
wait_desktop "$persistent_b"
assert_desktop_profile "$persistent_b" bare
assert_persistent_home_isolated "$persistent_b"
stop_container "$persistent_b"
fi

hardened="${prefix}-hardened"
start_container "$hardened" bare hardened \
  "${prefix}-hardened-home" "${prefix}-hardened-workspace"
wait_desktop "$hardened"
assert_desktop_profile "$hardened" bare
test "$(docker inspect "$hardened" --format '{{.HostConfig.ReadonlyRootfs}}')" = true
docker inspect "$hardened" --format '{{json .HostConfig.SecurityOpt}}' \
  | jq -e 'index("no-new-privileges:true") != null' >/dev/null
run_application_matrix "$hardened" bare hardened
stop_container "$hardened"

printf 'desktop application image tests passed: %s (exact base %s; input %s)\n' \
  "$image" "$base_image_id" "$image_ref"
