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
  conformance/LICENSE
  conformance/NOTICE
  conformance/v1/manifest.json
  packages/typescript/LICENSE
  packages/typescript/NOTICE
  packages/typescript/package.json
  packages/typescript/package-lock.json
  packages/typescript/scripts/clean-dist.mjs
  packages/typescript/scripts/conformance-adapter.mjs
  packages/typescript/scripts/package-allowlist.json
  packages/typescript/scripts/verify-package.mjs
  packages/typescript/src/index.ts
  packages/typescript/test/conformance-package.test.ts
  packages/typescript/test/hardening.test.ts
  packages/typescript/test/sdk.test.ts
  packages/typescript/tsconfig.json
  packages/python/LICENSE
  packages/python/NOTICE
  packages/python/MANIFEST.in
  packages/python/PACKAGE_ALLOWLIST.txt
  packages/python/SDIST_ALLOWLIST.txt
  packages/python/WHEEL_ALLOWLIST.txt
  packages/python/pyproject.toml
  packages/python/requirements-test.lock
  packages/python/scripts/run_conformance.py
  packages/python/scripts/verify_dist.py
  packages/python/src/xenoteer/__init__.py
  packages/python/src/xenoteer/py.typed
  packages/python/tests/test_hardening.py
  packages/python/tests/test_sdk.py
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
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/critical-shutdown-coordinator
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/shutdown-daemon-ready
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/machine-id
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/xauthority
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/xvfb
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/xenoteer-processd
  container/rootfs/etc/s6-overlay/s6-rc.d/user/contents.d/xenoteerd
  container/rootfs/etc/at-spi2/accessibility.conf
  container/rootfs/etc/dbus-1/session-local.conf
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
  container/rootfs/usr/local/libexec/xenoteer/request-critical-shutdown
  container/rootfs/usr/local/libexec/xenoteer/run-critical-shutdown-coordinator
  container/rootfs/usr/local/libexec/xenoteer/probe-viewer-protocol
  scripts/container/assert-idle-runtime.sh
  scripts/container/test-phase3-control-plane.sh
  scripts/container/test-phase3-websocket.py
  scripts/container/test-phase4-event-flood.py
  scripts/container/test-phase4-event-flood.sh
  scripts/container/test-phase4-live-fixtures.py
  scripts/container/test-phase5-atspi-live.py
  scripts/conformance/validate.py
  scripts/conformance/run.py
  scripts/conformance/tests/test_tools.py
  scripts/packages/verify-boundaries.py
  scripts/packages/tests/test_verify_boundaries.py
  scripts/sdk/README.md
  scripts/sdk/public_quickstarts.py
  scripts/sdk/quickstarts/python/quickstart.py
  scripts/sdk/quickstarts/rust/main.rs
  scripts/sdk/quickstarts/typescript/quickstart.mjs
  scripts/sdk/test-public-quickstarts.py
  scripts/sdk/test-phase6-ci-contract.py
  scripts/sdk/tests/test_public_quickstarts.py
  fixtures/x11/src/bin/x11-window-churn.rs
  container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/phase4-atspi-text.py
  container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/phase4-clipboard.py
  container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/phase5-atspi-stress.py
  container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/phase5-chromium-reload.py
  scripts/container/test-idle-soak.sh
  scripts/container/test-viewer-denial.sh
  container/spikes/novnc/tests/test_rfb_websocket_probe.py
  scripts/licenses/generate-debian-installed-manifest.sh
)

for path in "${required[@]}"; do
  if [[ ! -e "$path" ]]; then
    printf 'missing required container file: %s\n' "$path" >&2
    exit 1
  fi
done

bash -n scripts/container/test-phase3-control-plane.sh
grep -Fxq '# SPDX-License-Identifier: BUSL-1.1' \
  scripts/container/test-phase3-control-plane.sh
python3 -c 'import ast, pathlib; ast.parse(pathlib.Path("scripts/container/test-phase3-websocket.py").read_text())'
grep -Fxq '# SPDX-License-Identifier: BUSL-1.1' \
  scripts/container/test-phase3-websocket.py
for phase4_python in \
  scripts/container/test-phase4-event-flood.py \
  scripts/container/test-phase4-live-fixtures.py \
  container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/phase4-atspi-text.py \
  container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/phase4-clipboard.py; do
  python3 -c 'import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())' \
    "$phase4_python"
  grep -Fxq '# SPDX-License-Identifier: BUSL-1.1' "$phase4_python"
done
for phase5_python in \
  scripts/container/test-phase5-atspi-live.py \
  container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/phase5-atspi-stress.py \
  container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/phase5-chromium-reload.py; do
  python3 -c 'import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())' \
    "$phase5_python"
  grep -Fxq '# SPDX-License-Identifier: BUSL-1.1' "$phase5_python"
done
for package_boundary_python in \
  scripts/packages/verify-boundaries.py \
  scripts/packages/tests/test_verify_boundaries.py; do
  python3 -c 'import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())' \
    "$package_boundary_python"
  grep -Fxq '# SPDX-License-Identifier: Apache-2.0' "$package_boundary_python"
done
timeout 10s env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s scripts/packages/tests -p 'test_*.py'
timeout 30s env PYTHONDONTWRITEBYTECODE=1 python3 \
  scripts/packages/verify-boundaries.py
for conformance_python in \
  scripts/conformance/validate.py \
  scripts/conformance/run.py \
  scripts/conformance/tests/test_tools.py; do
  python3 -c 'import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())' \
    "$conformance_python"
  grep -Fxq '# SPDX-License-Identifier: Apache-2.0' "$conformance_python"
done
cmp -s conformance/LICENSE schemas/LICENSE
timeout 10s env PYTHONDONTWRITEBYTECODE=1 python3 \
  scripts/conformance/validate.py
timeout 10s env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s scripts/conformance/tests -p 'test_*.py'
for typescript_package_script in \
  packages/typescript/scripts/clean-dist.mjs \
  packages/typescript/scripts/conformance-adapter.mjs \
  packages/typescript/scripts/verify-package.mjs; do
  grep -Fq '// SPDX-License-Identifier: Apache-2.0' \
    "$typescript_package_script"
done
jq -e 'type == "array" and all(.[]; type == "string")' \
  packages/typescript/scripts/package-allowlist.json >/dev/null
for python_package_script in \
  packages/python/scripts/run_conformance.py \
  packages/python/scripts/verify_dist.py; do
  python3 -c 'import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())' \
    "$python_package_script"
  grep -Fxq '# SPDX-License-Identifier: Apache-2.0' "$python_package_script"
done
python3 -c 'import ast, pathlib; ast.parse(pathlib.Path("scripts/sdk/test-phase6-ci-contract.py").read_text())'
grep -Fxq '# SPDX-License-Identifier: BUSL-1.1' \
  scripts/sdk/test-phase6-ci-contract.py
timeout 10s env PYTHONDONTWRITEBYTECODE=1 python3 \
  scripts/sdk/test-phase6-ci-contract.py
for public_quickstart_python in \
  scripts/sdk/public_quickstarts.py \
  scripts/sdk/test-public-quickstarts.py \
  scripts/sdk/tests/test_public_quickstarts.py; do
  python3 -c 'import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())' \
    "$public_quickstart_python"
  grep -Fxq '# SPDX-License-Identifier: BUSL-1.1' "$public_quickstart_python"
done
python3 -c 'import ast, pathlib; ast.parse(pathlib.Path("scripts/sdk/quickstarts/python/quickstart.py").read_text())'
grep -Fxq '# SPDX-License-Identifier: Apache-2.0' \
  scripts/sdk/quickstarts/python/quickstart.py
grep -Fq '// SPDX-License-Identifier: Apache-2.0' \
  scripts/sdk/quickstarts/rust/main.rs
grep -Fq '// SPDX-License-Identifier: Apache-2.0' \
  scripts/sdk/quickstarts/typescript/quickstart.mjs
timeout 10s env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s scripts/sdk/tests -p 'test_*.py'
bash -n scripts/container/test-phase4-event-flood.sh
grep -Fxq '# SPDX-License-Identifier: BUSL-1.1' \
  scripts/container/test-phase4-event-flood.sh
sh -n tests/platform/run-x11-spikes.sh
grep -Fq -- '-nolisten tcp -noreset -auth' tests/platform/run-x11-spikes.sh
bash -n scripts/container/test-viewer-denial.sh
python3 -c 'import ast, pathlib; ast.parse(pathlib.Path("container/spikes/novnc/rfb_websocket_probe.py").read_text())'
python3 -c 'import ast, pathlib; ast.parse(pathlib.Path("container/spikes/novnc/tests/test_rfb_websocket_probe.py").read_text())'
timeout 10s env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  container/spikes/novnc/tests/test_rfb_websocket_probe.py
grep -Fq '"--cpus",' scripts/container/test-phase4-live-fixtures.py
grep -Fq '"2",' scripts/container/test-phase4-live-fixtures.py
grep -Fq 'f"{self.daemon_override}:/usr/local/bin/xenoteerd:ro"' \
  scripts/container/test-phase4-live-fixtures.py
grep -Fq 'if os.geteuid() == 0 and daemon_override is not None:' \
  scripts/container/test-phase4-live-fixtures.py
grep -Fq 'os.chown(token_file, 1000, 1000)' \
  scripts/container/test-phase4-live-fixtures.py
grep -Fq 'scripts/container/test-phase4-live-fixtures.py xenoteer:desktop-apps-test' \
  .github/workflows/ci.yml
grep -Fq 'scripts/container/test-phase4-event-flood.sh xenoteer:phase2' \
  .github/workflows/ci.yml
grep -Fq 'scripts/container/test-phase5-atspi-live.py xenoteer:desktop-apps-test' \
  .github/workflows/ci.yml
if grep -Fq 'XENOTEER_TEST_DAEMON_BINARY' .github/workflows/ci.yml; then
  printf 'CI Phase 5 acceptance must test the exact immutable image\n' >&2
  exit 1
fi
grep -Fq '"--cpus",' scripts/container/test-phase5-atspi-live.py
grep -Fq '"--memory",' scripts/container/test-phase5-atspi-live.py
grep -Fq '"--pids-limit",' scripts/container/test-phase5-atspi-live.py
grep -Fq '"--shm-size",' scripts/container/test-phase5-atspi-live.py
grep -Fq '2000000000 6442450944 512 4294967296' \
  scripts/container/test-phase5-atspi-live.py
grep -Fq 'timeout-minutes: 10' .github/workflows/ci.yml
grep -Fq 'build --quiet --release --locked --jobs 2' \
  scripts/container/test-phase4-event-flood.sh
grep -Fq -- '--cpus 2' scripts/container/test-phase4-event-flood.sh
if grep -Fq 'XENOTEERD_BINARY_OVERRIDE' .github/workflows/ci.yml; then
  printf 'CI event-flood acceptance must test the exact immutable image\n' >&2
  exit 1
fi
grep -Fq 'for binary in xenoteerd xenoteer-processd; do' \
  scripts/licenses/inventory-image-first-party.sh
grep -Fq 'for config in /etc/at-spi2/accessibility.conf /etc/dbus-1/session-local.conf; do' \
  scripts/licenses/inventory-image-first-party.sh
grep -Fxq $'/usr/local/bin/xenoteer-processd\tBUSL-1.1\t/usr/share/doc/xenoteer/LICENSE' \
  container/licenses/image-first-party-paths.tsv
for bus_config in \
  container/rootfs/etc/at-spi2/accessibility.conf \
  container/rootfs/etc/dbus-1/session-local.conf; do
  grep -Fq '<allow user="1001"/>' "$bus_config"
  grep -Fq '<auth>EXTERNAL</auth>' "$bus_config" \
    || [[ $bus_config == */session-local.conf ]]
done
phase3_err_trap_self_test=$(scripts/container/test-phase3-control-plane.sh \
  --self-test-err-trap)
grep -Fq 'Phase 3 control-plane ERR trap self-test passed' \
  <<<"$phase3_err_trap_self_test"

# The desktop and daemon deliberately have different UIDs. AT-SPI toolkit P2P
# servers reject cross-UID EXTERNAL authentication, so the Rust adapter must use
# the policy-controlled central accessibility bus for trees, actions, and events.
atspi_features=$(cargo tree -p xenoteer-atspi --all-features -e features)
for forbidden_feature in \
  'atspi-connection feature "default"' \
  'atspi-connection feature "p2p"' \
  'zbus feature "p2p"'; do
  if grep -Fq "$forbidden_feature" <<<"$atspi_features"; then
    printf 'forbidden cross-UID AT-SPI P2P feature is active: %s\n' \
      "$forbidden_feature" >&2
    exit 1
  fi
done
if grep -Eq '(^|[[:space:]])atspi v[0-9]' <<<"$atspi_features"; then
  printf 'the atspi facade re-enabled default P2P features\n' >&2
  exit 1
fi

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
if ! awk -F '\t' '$1 == "scripts/packages/verify-boundaries.py" && $3 == "Apache-2.0" && $4 == "crates/xenoteer-protocol/LICENSE|crates/xenoteer-protocol/NOTICE|crates/xenoteer-sdk/LICENSE|crates/xenoteer-sdk/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Cargo package-boundary tooling is not classified as Apache-2.0\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "conformance/v1/manifest.json" && $3 == "Apache-2.0" && $4 == "conformance/LICENSE|conformance/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Conformance corpus is not classified at its Apache-2.0 boundary\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "scripts/conformance/validate.py" && $3 == "Apache-2.0" && $4 == "conformance/LICENSE|conformance/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Conformance tooling is not classified at its Apache-2.0 boundary\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "packages/typescript/src/index.ts" && $3 == "Apache-2.0" && $4 == "packages/typescript/LICENSE|packages/typescript/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'TypeScript SDK is not classified at its Apache-2.0 package boundary\n' >&2
  exit 1
fi
for typescript_package_path in \
  packages/typescript/package.json \
  packages/typescript/package-lock.json \
  packages/typescript/scripts/clean-dist.mjs \
  packages/typescript/scripts/conformance-adapter.mjs \
  packages/typescript/scripts/package-allowlist.json \
  packages/typescript/scripts/verify-package.mjs \
  packages/typescript/tsconfig.json; do
  if ! awk -F '\t' -v path="$typescript_package_path" \
    '$1 == path && $3 == "Apache-2.0" && $4 == "packages/typescript/LICENSE|packages/typescript/NOTICE" { found = 1 } END { exit !found }' \
    /tmp/xenoteer-first-party.tsv; then
    printf 'TypeScript package boundary file is not classified as Apache-2.0: %s\n' \
      "$typescript_package_path" >&2
    exit 1
  fi
done
if ! awk -F '\t' '$1 == "packages/python/src/xenoteer/__init__.py" && $3 == "Apache-2.0" && $4 == "packages/python/LICENSE|packages/python/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Python SDK is not classified at its Apache-2.0 package boundary\n' >&2
  exit 1
fi
for python_package_path in \
  packages/python/MANIFEST.in \
  packages/python/PACKAGE_ALLOWLIST.txt \
  packages/python/SDIST_ALLOWLIST.txt \
  packages/python/WHEEL_ALLOWLIST.txt \
  packages/python/pyproject.toml \
  packages/python/requirements-test.lock \
  packages/python/scripts/run_conformance.py \
  packages/python/scripts/verify_dist.py; do
  if ! awk -F '\t' -v path="$python_package_path" \
    '$1 == path && $3 == "Apache-2.0" && $4 == "packages/python/LICENSE|packages/python/NOTICE" { found = 1 } END { exit !found }' \
    /tmp/xenoteer-first-party.tsv; then
    printf 'Python package boundary file is not classified as Apache-2.0: %s\n' \
      "$python_package_path" >&2
    exit 1
  fi
done
if ! awk -F '\t' '$1 == "scripts/sdk/test-phase6-ci-contract.py" && $3 == "BUSL-1.1" && $4 == "LICENSE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Root SDK CI contract test is absent from the BSL source inventory\n' >&2
  exit 1
fi
for public_quickstart_path in \
  scripts/sdk/README.md \
  scripts/sdk/public_quickstarts.py \
  scripts/sdk/test-public-quickstarts.py \
  scripts/sdk/tests/test_public_quickstarts.py; do
  if ! awk -F '\t' -v path="$public_quickstart_path" \
    '$1 == path && $3 == "BUSL-1.1" && $4 == "LICENSE" { found = 1 } END { exit !found }' \
    /tmp/xenoteer-first-party.tsv; then
    printf 'Public quick-start gate file is absent from the BSL source inventory: %s\n' \
      "$public_quickstart_path" >&2
    exit 1
  fi
done
if ! awk -F '\t' '$1 == "scripts/sdk/quickstarts/rust/main.rs" && $3 == "Apache-2.0" && $4 == "crates/xenoteer-sdk/LICENSE|crates/xenoteer-sdk/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Rust public quick-start is not classified at the SDK Apache boundary\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "scripts/sdk/quickstarts/typescript/quickstart.mjs" && $3 == "Apache-2.0" && $4 == "packages/typescript/LICENSE|packages/typescript/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'TypeScript public quick-start is not classified at the SDK Apache boundary\n' >&2
  exit 1
fi
if ! awk -F '\t' '$1 == "scripts/sdk/quickstarts/python/quickstart.py" && $3 == "Apache-2.0" && $4 == "packages/python/LICENSE|packages/python/NOTICE" { found = 1 } END { exit !found }' \
  /tmp/xenoteer-first-party.tsv; then
  printf 'Python public quick-start is not classified at the SDK Apache boundary\n' >&2
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
grep -Fq 'finish-critical xenoteer-processd "$@"' \
  container/rootfs/etc/s6-overlay/s6-rc.d/xenoteer-processd/finish
grep -Fq 'finish-critical xvfb "$@"' \
  container/rootfs/etc/s6-overlay/s6-rc.d/xvfb/finish
grep -Fq '/run/s6-linux-init-container-results' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Fq '/run/s6/basedir/bin/halt' \
  container/rootfs/usr/local/libexec/xenoteer/request-critical-shutdown
grep -Fq '/command/s6-svscanctl -t /run/service' \
  container/rootfs/usr/local/libexec/xenoteer/request-critical-shutdown
grep -Fq 'critical-shutdown-claimed' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Eq '^exit 125$' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
# The static assertion requires the literal variable.
# shellcheck disable=SC2016
grep -Fq 'maintenance_marker=$maintenance_parent/$service' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Fq '/command/s6-svstat -o wantedup .' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Fq 'parent_attributes" = 0:0:700' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Fq 'marker_attributes" = 0:0:600' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
# These assertions require the literal shell variables.
# shellcheck disable=SC2016
grep -Fq '[ ! -L "$maintenance_parent" ]' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
# shellcheck disable=SC2016
grep -Fq '[ ! -L "$maintenance_marker" ]' \
  container/rootfs/usr/local/libexec/xenoteer/finish-critical
grep -Fq '"-wD",' scripts/container/test-phase5-atspi-live.py
grep -Fq '"12000",' scripts/container/test-phase5-atspi-live.py
grep -Fq 'state == ["false", "false"]' scripts/container/test-phase5-atspi-live.py
# The static ordering assertion requires the literal shell variable.
# shellcheck disable=SC2016
if [[ $(grep -nF 'maintenance_marker=$maintenance_parent/$service' \
    container/rootfs/usr/local/libexec/xenoteer/finish-critical | cut -d: -f1) \
    -ge $(grep -nF '/command/s6-svstat -o wantedup .' \
    container/rootfs/usr/local/libexec/xenoteer/finish-critical | cut -d: -f1) ]]; then
  printf 'AT-SPI maintenance must be consumed before supervision intent is classified\n' >&2
  exit 1
fi
grep -Fq 'rm -f /usr/share/dbus-1/services/org.a11y.Bus.service' Dockerfile
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
