#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"

scripts/container/test-desktop-profiles.sh

package_lock=container/fixtures/desktop-apps/packages.lock
artifact_lock=container/fixtures/desktop-apps/artifacts.lock
test -s "$package_lock"
test -s "$artifact_lock"
if grep -Ev '^(#.*|$|[a-z0-9][a-z0-9.+-]*=[^[:space:]=]+)$' "$package_lock" | grep -q .; then
  printf 'desktop-app fixture package lock contains an invalid or unpinned entry\n' >&2
  exit 1
fi
test "$(sed '/^#/d; /^$/d' "$package_lock" | cut -d= -f1 | LC_ALL=C sort -u | wc -l)" \
  -eq "$(sed '/^#/d; /^$/d' "$package_lock" | wc -l)"
for package in chromium chromium-sandbox firefox-esr gir1.2-gtk-3.0 python3-gi python3-pyatspi \
  python3-pyqt6 python3-pyqt6.qtwebengine python3-websocket wmctrl; do
  grep -Eq "^${package}=[^=[:space:]]+$" "$package_lock"
done

grep -Fxq 'ELECTRON_VERSION=43.1.1' "$artifact_lock"
grep -Fxq \
  'ELECTRON_LINUX_X64_URL=https://github.com/electron/electron/releases/download/v43.1.1/electron-v43.1.1-linux-x64.zip' \
  "$artifact_lock"
grep -Eq '^ELECTRON_LINUX_X64_SHA256=[0-9a-f]{64}$' "$artifact_lock"
test "$(grep -Ec '^ELECTRON_[A-Z0-9_]+=' "$artifact_lock")" -eq 3

grep -Fq 'ARG XENOTEER_BASE_IMAGE=xenoteer:dev' \
  container/fixtures/desktop-apps/Dockerfile
# The Dockerfile variable must remain literal.
# shellcheck disable=SC2016
grep -Fq 'FROM ${XENOTEER_BASE_IMAGE}' container/fixtures/desktop-apps/Dockerfile
grep -Fq 'test-only-non-distributable' container/fixtures/desktop-apps/Dockerfile

if rg -n --glob 'compose*.yml' --glob 'container/fixtures/desktop-apps/Dockerfile' \
  -- '(^|[[:space:]])(--no-sandbox|--disable-dev-shm-usage|--privileged)([[:space:]]|$)' \
  >/dev/null; then
  printf 'runtime profile contains a forbidden browser/container escape\n' >&2
  exit 1
fi

if ! docker compose version >/dev/null 2>&1; then
  printf 'docker compose is required for runtime profile validation\n' >&2
  exit 77
fi

dev_config=$(XENOTEER_TOKEN_FILE=/dev/null docker compose \
  -f compose.dev.yml --profile '*' config --format json)
hardened_config=$(XENOTEER_TOKEN_FILE=/dev/null docker compose \
  -f compose.dev.yml -f compose.hardened.yml --profile '*' config --format json)

jq -e '
  .services.xenoteer as $service |
  ($service.ports | length) == 1 and
  $service.ports[0].host_ip == "127.0.0.1" and
  $service.ports[0].target == 8080 and
  $service.ports[0].protocol == "tcp" and
  ($service.expose // []) == [] and
  $service.shm_size == "4294967296" and
  $service.environment.DESKTOP_PROFILE == "bare" and
  ($service.security_opt | index("seccomp=./container/spikes/browser/seccomp_profile.json")) != null and
  ($service.privileged // false) == false and
  ($service.network_mode // "") != "host" and
  ($service.pid // "") != "host" and
  ($service.ipc // "") != "host" and
  ($service.secrets | length) == 1 and
  $service.secrets[0].source == "xenoteer_api_token" and
  $service.secrets[0].target == "xenoteer_api_token" and
  ($service.secrets[0] | has("uid") | not) and
  ($service.secrets[0] | has("gid") | not) and
  ($service.secrets[0] | has("mode") | not)
' <<<"$dev_config" >/dev/null

if sed -n '/^[[:space:]]*secrets:/,/^[^[:space:]]/p' compose.dev.yml \
  | grep -Eq '^[[:space:]]+(uid|gid|mode):'; then
  printf 'file-backed Compose secrets must not claim portable uid/gid/mode mutation\n' >&2
  exit 1
fi

jq -e '
  .services.xenoteer as $service |
  $service.read_only == true and
  $service.cap_drop == ["ALL"] and
  ($service.cap_add | sort) ==
    ["CHOWN", "DAC_OVERRIDE", "FOWNER", "KILL", "SETGID", "SETUID", "SYS_CHROOT"] and
  ($service.security_opt | index("no-new-privileges:true")) != null and
  ($service.security_opt | index("seccomp=./container/spikes/browser/seccomp_profile.json")) != null and
  ($service.tmpfs | any(startswith("/run:") and contains("size=512m") and contains("exec"))) and
  ($service.tmpfs | any(startswith("/tmp:") and contains("size=1g") and contains("noexec"))) and
  $service.shm_size == "4294967296" and
  ($service.privileged // false) == false
' <<<"$hardened_config" >/dev/null

printf 'runtime profile tests passed\n'
