#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

if [[ $# -lt 1 || $# -gt 3 ]]; then
  printf 'usage: %s CONTAINER [bare|standard] [0|1]\n' "$0" >&2
  exit 64
fi

container=$1
profile=${2:-bare}
viewer_enabled=${3:-1}
case "$profile" in bare|standard) ;; *) exit 64 ;; esac
case "$viewer_enabled" in 0|1) ;; *) exit 64 ;; esac

# Do not inventory an s6 startup transaction. Require the complete recurring
# readiness contract first, with a bounded allowance for the image to boot.
for _ in {1..60}; do
  if docker exec "$container" /usr/local/libexec/xenoteer/healthcheck >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [[ ${ready:-0} != 1 ]]; then
  printf '%s did not become ready before idle inventory\n' "$container" >&2
  exit 1
fi

# The probe process is root and excludes itself. Retry boundedly so Docker's
# short-lived HEALTHCHECK commands cannot create a false process-inventory
# failure; any persistent or resurrected process remains a hard failure.
docker exec --interactive "$container" python3 - "$profile" "$viewer_enabled" <<'PY'
from __future__ import annotations

import collections
import ipaddress
import os
import pathlib
import socket
import sys
import time

profile, viewer_enabled = sys.argv[1:]
self_pid = os.getpid()

expected_user = collections.Counter(
    {
        "Xvfb": 1,
        "at-spi-bus-laun": 1,
        "at-spi2-registr": 1,
        "dbus-daemon": 2,
        "dconf-service": 1,
        "xfce4-session": 1,
        "xfconfd": 1,
        "xfdesktop": 1,
        "xfsettingsd": 1,
        "xfwm4": 1,
        "xenoteerd": 1,
    }
)
if profile == "standard":
    expected_user["xfce4-panel"] = 1
if viewer_enabled == "1":
    expected_user["X0tigervnc"] = 1
    expected_user["websockify"] = 1
else:
    expected_user["s6-pause"] = 2

expected_root = collections.Counter(
    {
        "s6-ipcserverd": 1,
        "s6-linux-init-s": 1,
        "run-critical-sh": 1,
        "s6-supervise": 11,
        "s6-svscan": 1,
    }
)


def process_inventory() -> tuple[
    collections.Counter[str], collections.Counter[str], list[tuple[str, str]]
]:
    root: collections.Counter[str] = collections.Counter()
    user: collections.Counter[str] = collections.Counter()
    unacceptable: list[tuple[str, str]] = []
    for process in pathlib.Path("/proc").glob("[0-9]*"):
        try:
            pid = int(process.name)
            if pid == self_pid:
                continue
            status = (process / "status").read_text()
            uid = int(
                next(line for line in status.splitlines() if line.startswith("Uid:"))
                .split()[1]
            )
            state = next(
                line for line in status.splitlines() if line.startswith("State:")
            ).split()[1]
            comm = (process / "comm").read_text().strip()
        except (FileNotFoundError, PermissionError, StopIteration, ValueError):
            continue
        if state in {"T", "t", "X", "Z"}:
            unacceptable.append((comm, state))
            continue
        if uid == 0:
            root[comm] += 1
        elif uid == 1000:
            user[comm] += 1
        else:
            raise SystemExit(f"idle runtime contains unexpected UID {uid}")
    return root, user, unacceptable


last_root: collections.Counter[str] = collections.Counter()
last_user: collections.Counter[str] = collections.Counter()
last_unacceptable: list[tuple[str, str]] = []
for _ in range(100):
    last_root, last_user, last_unacceptable = process_inventory()
    if (
        not last_unacceptable
        and last_root == expected_root
        and last_user == expected_user
    ):
        break
    time.sleep(0.1)
else:
    raise SystemExit(
        "idle process allowlist differs: "
        f"root={dict(sorted(last_root.items()))!r}, "
        f"user={dict(sorted(last_user.items()))!r}, "
        f"unacceptable_states={last_unacceptable!r}"
    )

expected_listeners = {("0.0.0.0", 8080)}
if viewer_enabled == "1":
    expected_listeners.update({("127.0.0.1", 5900), ("127.0.0.1", 6080)})
observed_listeners: set[tuple[str, int]] = set()
for table in ("/proc/net/tcp", "/proc/net/tcp6"):
    for line in pathlib.Path(table).read_text().splitlines()[1:]:
        fields = line.split()
        if fields[3] != "0A":
            continue
        address, port_hex = fields[1].split(":")
        if table.endswith("tcp"):
            host = socket.inet_ntoa(bytes.fromhex(address)[::-1])
        else:
            packed = b"".join(
                bytes.fromhex(address[offset : offset + 8])[::-1]
                for offset in range(0, 32, 8)
            )
            host = socket.inet_ntop(socket.AF_INET6, packed)
        observed_listeners.add((str(ipaddress.ip_address(host)), int(port_hex, 16)))
if observed_listeners != expected_listeners:
    raise SystemExit(
        "idle listener allowlist differs: "
        f"observed={sorted(observed_listeners)!r}, "
        f"expected={sorted(expected_listeners)!r}"
    )

observed_udp: set[tuple[str, int]] = set()
for table in ("/proc/net/udp", "/proc/net/udp6"):
    for line in pathlib.Path(table).read_text().splitlines()[1:]:
        fields = line.split()
        address, port_hex = fields[1].split(":")
        if table.endswith("udp"):
            host = socket.inet_ntoa(bytes.fromhex(address)[::-1])
        else:
            packed = b"".join(
                bytes.fromhex(address[offset : offset + 8])[::-1]
                for offset in range(0, 32, 8)
            )
            host = socket.inet_ntop(socket.AF_INET6, packed)
        observed_udp.add((str(ipaddress.ip_address(host)), int(port_hex, 16)))
if observed_udp:
    raise SystemExit(f"idle runtime contains UDP listeners: {sorted(observed_udp)!r}")

for path in ("/run/user/1000/bus", "/run/user/1000/at-spi/bus_99"):
    if not pathlib.Path(path).is_socket():
        raise SystemExit(f"required runtime socket is absent: {path}")
if pathlib.Path("/run/dbus/system_bus_socket").exists():
    raise SystemExit("unexpected system D-Bus socket exists")
PY

if [[ $viewer_enabled == 1 ]]; then
  docker exec --user 1000 "$container" \
    /command/s6-envdir -f -L /run/xenoteer/env \
      /usr/local/libexec/xenoteer/probe-viewer-protocol
fi
