#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"

required=(
  Dockerfile
  NOTICE
  crates/xenoteer-protocol/LICENSE
  crates/xenoteer-protocol/NOTICE
  crates/xenoteer-sdk/LICENSE
  crates/xenoteer-sdk/NOTICE
  schemas/LICENSE
  schemas/NOTICE
  compose.dev.yml
  compose.hardened.yml
  container/locks/release.lock
  container/locks/sources.lock
  container/locks/novnc-critical-assets.sha256
  container/novnc/mandatory.json
  container/packages/desktop.txt
  container/packages/viewer.txt
  container/spikes/browser/licenses/moby/LICENSE
  container/spikes/browser/licenses/moby/NOTICE
  container/spikes/browser/licenses/playwright/LICENSE
  container/spikes/browser/licenses/playwright/NOTICE
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/runtime-directories
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/machine-id
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/xauthority
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/xvfb
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/xenoteerd
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
  container/rootfs/usr/local/libexec/xenoteer/probe-viewer-protocol
  scripts/container/assert-idle-runtime.sh
  scripts/container/test-idle-soak.sh
  scripts/container/test-viewer-denial.sh
  scripts/licenses/generate-debian-installed-manifest.sh
)

for path in "${required[@]}"; do
  if [[ ! -e "$path" ]]; then
    printf 'missing required container file: %s\n' "$path" >&2
    exit 1
  fi
done

assert_package_group() {
  local file=$1
  shift
  local expected actual
  expected=$(printf '%s\n' "$@" | LC_ALL=C sort)
  actual=$(sed '/^#/d; /^$/d' "$file" | LC_ALL=C sort)
  if [[ $actual != "$expected" ]]; then
    printf 'production package group differs from its reviewed contract: %s\n' "$file" >&2
    diff -u <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") >&2 || true
    exit 1
  fi
}

assert_package_group container/packages/runtime.txt \
  ca-certificates curl dbus dbus-bin dbus-daemon dbus-user-session \
  libglib2.0-bin libxkbcommon-x11-0 locales procps tzdata util-linux \
  x11-utils x11-xserver-utils xauth xkb-data xvfb
assert_package_group container/packages/desktop.txt \
  adwaita-icon-theme at-spi2-core dconf-gsettings-backend fontconfig \
  fonts-dejavu-core fonts-liberation2 fonts-noto-cjk fonts-noto-color-emoji \
  fonts-noto-core fonts-noto-mono greybird-gtk-theme hicolor-icon-theme \
  libatk-bridge2.0-0t64 librsvg2-common xfce4-panel xfce4-session \
  xfce4-settings xfconf xfdesktop4 xfwm4
assert_package_group container/packages/viewer.txt \
  python3-websockify tigervnc-scraping-server

if sed '/^#/d; /^$/d' container/packages/{runtime,desktop,viewer}.txt \
    | grep -Ex '(dbus-x11|novnc|websockify|nodejs|net-tools|thunar|tumbler|lightdm|gdm3|sddm|xfce4-power-manager|xfce4-notifyd)' >/dev/null; then
  printf 'forbidden package entered a production package group\n' >&2
  exit 1
fi

jq -e '
  . == {"resize":"scale","shared":true,"view_clip":false,"view_only":true}
' container/novnc/mandatory.json >/dev/null

if find . -type d -name __pycache__ -o -type f -name '*.pyc' | grep -q .; then
  printf 'generated Python bytecode is present in the source tree\n' >&2
  exit 1
fi

scripts/container/validate-locks.sh
scripts/container/check-s6-graph.sh
scripts/container/test-runtime-scripts.sh
scripts/container/test-runtime-profiles.sh
scripts/licenses/inventory-first-party.sh . /tmp/xenoteer-first-party.tsv
for workflow_state in .codex/TODO.md .codex/DETAILS.md; do
  if ! awk -F '\t' -v path="$workflow_state" '$1 == path && $3 == "BUSL-1.1" { found = 1 } END { exit !found }' \
    /tmp/xenoteer-first-party.tsv; then
    printf 'tracked implementation state is absent from source inventory: %s\n' "$workflow_state" >&2
    exit 1
  fi
done
if ! awk -F '\t' '$1 == "crates/xenoteer-protocol/src/lib.rs" && $3 == "Apache-2.0" && $4 == "crates/xenoteer-protocol/LICENSE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Protocol source is not classified at the Apache-2.0 package boundary\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "crates/xenoteer-sdk/src/lib.rs" && $3 == "Apache-2.0" && $4 == "crates/xenoteer-sdk/LICENSE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Rust SDK source is not classified at the Apache-2.0 package boundary\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "schemas/v1/capabilities.json" && $3 == "Apache-2.0" && $4 == "schemas/LICENSE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Checked-in schemas are not classified at their self-contained Apache-2.0 boundary\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "container/spikes/browser/docker-default-seccomp.json" && $3 == "Apache-2.0" && $4 == "container/spikes/browser/licenses/moby/LICENSE|container/spikes/browser/licenses/moby/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Moby-derived Docker seccomp baseline is not classified as Apache-2.0\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "container/spikes/browser/seccomp_profile.json" && $3 == "Apache-2.0" && $4 == "container/spikes/browser/licenses/moby/LICENSE|container/spikes/browser/licenses/moby/NOTICE|container/spikes/browser/licenses/playwright/LICENSE|container/spikes/browser/licenses/playwright/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Moby/Playwright-derived browser seccomp profile is not classified as Apache-2.0\n' >&2
  exit 1
fi
scripts/licenses/generate-source-sbom.sh /tmp/xenoteer-first-party.tsv /tmp/xenoteer-source.spdx.json
scripts/licenses/test-inventory-pruning.sh
scripts/licenses/test-final-inventory.sh
scripts/licenses/test-novnc-manifest.sh
scripts/container/test-browser-seccomp.sh

jq -e '.spdxVersion == "SPDX-2.3" and (.files | length > 0)' \
  /tmp/xenoteer-source.spdx.json >/dev/null
rm -f /tmp/xenoteer-first-party.tsv /tmp/xenoteer-source.spdx.json

if command -v shellcheck >/dev/null 2>&1; then
  mapfile -d '' shell_files < <(
    rg --files-with-matches --null '^#!/bin/(ba)?sh([[:space:]]|$)' \
      scripts container/rootfs container/spikes
  )
  if ((${#shell_files[@]} == 0)); then
    printf 'no shell scripts were discovered for ShellCheck\n' >&2
    exit 1
  fi
  shellcheck -x "${shell_files[@]}"
else
  printf 'shellcheck is required for the blocking static gate\n' >&2
  exit 1
fi

if docker compose version >/dev/null 2>&1; then
  XENOTEER_TOKEN_FILE=/dev/null docker compose -f compose.dev.yml --profile '*' config --quiet
  XENOTEER_TOKEN_FILE=/dev/null docker compose -f compose.dev.yml -f compose.hardened.yml --profile '*' config --quiet
  dev_config=$(XENOTEER_TOKEN_FILE=/dev/null docker compose -f compose.dev.yml --profile '*' config --format json)
  hardened_config=$(XENOTEER_TOKEN_FILE=/dev/null docker compose \
    -f compose.dev.yml -f compose.hardened.yml --profile '*' config --format json)
  jq -e '
    (.services.xenoteer | has("build") | not)
      and (
        .services.xenoteer.security_opt
        | index("seccomp=./container/spikes/browser/seccomp_profile.json") != null
          and index("no-new-privileges:true") == null
      )
  ' <<<"$dev_config" >/dev/null
  jq -e '
    (.services.xenoteer | has("build") | not)
      and (
        .services.xenoteer.security_opt
        | index("seccomp=./container/spikes/browser/seccomp_profile.json") != null
          and index("no-new-privileges:true") != null
      )
  ' <<<"$hardened_config" >/dev/null
  jq -e '
    (.services.xenoteer.cap_add | sort)
      == ["CHOWN", "DAC_OVERRIDE", "FOWNER", "KILL", "SETGID", "SETUID", "SYS_CHROOT"]
  ' <<<"$hardened_config" >/dev/null
else
  printf 'docker compose is required for the blocking configuration-lint gate\n' >&2
  exit 1
fi

grep -Fq 'seccomp=./container/spikes/browser/seccomp_profile.json' compose.dev.yml
if rg -n '^[[:space:]]+build:' compose*.yml >/dev/null; then
  printf 'Compose must consume the lock-aware wrapper image, not build directly\n' >&2
  exit 1
fi
grep -Fq 'no-new-privileges:true' compose.hardened.yml
grep -Fq 'cap_add: [CHOWN, DAC_OVERRIDE, FOWNER, KILL, SETGID, SETUID, SYS_CHROOT]' \
  compose.hardened.yml

grep -Fq 'ENTRYPOINT ["/init"]' Dockerfile
grep -Fq 'HEALTHCHECK' Dockerfile
grep -Fq 'USER root' Dockerfile
test "$(grep -Ec '^EXPOSE ' Dockerfile)" -eq 1
grep -Fxq 'EXPOSE 8080' Dockerfile
grep -Fq "FROM \${DEBIAN_BASE_IMAGE} AS novnc-assets" Dockerfile
grep -Fq "dpkg-deb -x \"\$novnc_deb\" /tmp/novnc-unpack" Dockerfile
grep -Fq '/usr/share/doc/xenoteer/novnc-files.tsv' Dockerfile
grep -Fq '/usr/share/doc/xenoteer/debian-installed-files.tsv' Dockerfile
grep -Fq 'com.aeor.xenoteer.viewer.input-policy="server-side-view-only"' Dockerfile
inventory_layer_line=$(grep -nF \
  '&& nice -n 15 /usr/local/libexec/xenoteer/inventory-final-image / /usr/share/doc/xenoteer/final-files.tsv' \
  Dockerfile | cut -d: -f1)
label_line=$(grep -nF 'LABEL org.opencontainers.image.title="Xenoteer"' Dockerfile | cut -d: -f1)
if [[ -z $inventory_layer_line || -z $label_line || $label_line -le $inventory_layer_line ]]; then
  printf 'source-dependent OCI labels must follow all final rootfs assembly and inventory\n' >&2
  exit 1
fi
grep -Fq "assert_label org.opencontainers.image.revision \"\$revision\"" scripts/container/build.sh
grep -Fq "assert_label com.aeor.xenoteer.source.dirty \"\$source_dirty\"" scripts/container/build.sh
grep -Fq "assert_label com.aeor.xenoteer.source-tree.sha256 \"\$source_tree_sha256\"" scripts/container/build.sh
grep -Fq "assert_label com.aeor.xenoteer.dependency-lock.sha256 \"\$dependency_lock_sha256\"" scripts/container/build.sh
if rg -n '\bxdotool\b' Dockerfile container/packages container/rootfs >/dev/null; then
  printf 'xdotool is fixture-only and must not enter the production image path\n' >&2
  exit 1
fi
grep -Fq 'finish-critical xenoteerd "$@"' \
  container/rootfs/etc/s6-overlay/s6-rc.d/xenoteerd/finish
grep -Fq 'finish-critical xvfb "$@"' \
  container/rootfs/etc/s6-overlay/s6-rc.d/xvfb/finish
grep -Fq '/run/s6-linux-init-container-results' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Fq '/run/s6/basedir/bin/halt' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Eq '^exit 125$' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Fq '/command/s6-svstat -o wantedup .' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
if [[ -e container/rootfs/etc/s6-overlay/s6-rc.d/orderly-shutdown ]]; then
  printf 'readiness-dependent orderly-shutdown marker must not be present\n' >&2
  exit 1
fi
if grep -Eq -- '(^|[[:space:]])(-ac|--privileged|--no-sandbox)([[:space:]]|$)' Dockerfile compose*.yml; then
  printf 'forbidden insecure runtime option found\n' >&2
  exit 1
fi
if rg -n 'XENOTEER_(HTTP_ADDR|AUTH_TOKEN_FILE)' \
  Dockerfile compose*.yml container/rootfs >/dev/null; then
  printf 'legacy daemon environment key found; use the nested XENOTEER__ contract\n' >&2
  exit 1
fi
if rg -n 'XENOTEER_[A-Z]' container/rootfs >/dev/null; then
  printf 'single-underscore XENOTEER runtime key found; daemon config accepts only typed XENOTEER__ keys\n' >&2
  exit 1
fi

printf 'container static tests passed\n'
