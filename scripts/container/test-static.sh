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
)

for path in "${required[@]}"; do
  if [[ ! -e "$path" ]]; then
    printf 'missing required container file: %s\n' "$path" >&2
    exit 1
  fi
done

if find . -type d -name __pycache__ -o -type f -name '*.pyc' | grep -q .; then
  printf 'generated Python bytecode is present in the source tree\n' >&2
  exit 1
fi

scripts/container/validate-locks.sh
scripts/container/check-s6-graph.sh
scripts/container/test-runtime-scripts.sh
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
scripts/container/test-browser-seccomp.sh

jq -e '.spdxVersion == "SPDX-2.3" and (.files | length > 0)' \
  /tmp/xenoteer-source.spdx.json >/dev/null
rm -f /tmp/xenoteer-first-party.tsv /tmp/xenoteer-source.spdx.json

if command -v shellcheck >/dev/null 2>&1; then
  mapfile -d '' shell_files < <(
    find scripts container/rootfs container/spikes -type f \
      \( -name '*.sh' -o -path '*/run' -o -path '*/run-*' -o -path '*/finish' -o -path '*/check' \) \
      -print0
  )
  shellcheck -x "${shell_files[@]}"
else
  printf 'warning: shellcheck is not installed; shell lint skipped\n' >&2
fi

if docker compose version >/dev/null 2>&1; then
  XENOTEER_TOKEN_FILE=/dev/null docker compose -f compose.dev.yml --profile '*' config --quiet
  XENOTEER_TOKEN_FILE=/dev/null docker compose -f compose.dev.yml -f compose.hardened.yml --profile '*' config --quiet
  dev_config=$(XENOTEER_TOKEN_FILE=/dev/null docker compose -f compose.dev.yml --profile '*' config --format json)
  hardened_config=$(XENOTEER_TOKEN_FILE=/dev/null docker compose \
    -f compose.dev.yml -f compose.hardened.yml --profile '*' config --format json)
  jq -e '
    .services.xenoteer.security_opt
    | index("seccomp=./container/spikes/browser/seccomp_profile.json") != null
      and index("no-new-privileges:true") == null
  ' <<<"$dev_config" >/dev/null
  jq -e '
    .services.xenoteer.security_opt
    | index("seccomp=./container/spikes/browser/seccomp_profile.json") != null
      and index("no-new-privileges:true") != null
  ' <<<"$hardened_config" >/dev/null
  jq -e '
    (.services.xenoteer.cap_add | sort)
      == ["CHOWN", "DAC_OVERRIDE", "FOWNER", "KILL", "SETGID", "SETUID", "SYS_CHROOT"]
  ' <<<"$hardened_config" >/dev/null
else
  printf 'warning: docker compose is not installed; Compose parse skipped\n' >&2
fi

grep -Fq 'seccomp=./container/spikes/browser/seccomp_profile.json' compose.dev.yml
grep -Fq 'no-new-privileges:true' compose.hardened.yml
grep -Fq 'cap_add: [CHOWN, DAC_OVERRIDE, FOWNER, KILL, SETGID, SETUID, SYS_CHROOT]' \
  compose.hardened.yml

grep -Fq 'ENTRYPOINT ["/init"]' Dockerfile
grep -Fq 'HEALTHCHECK' Dockerfile
grep -Fq 'USER root' Dockerfile
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
