#!/bin/sh
# SPDX-License-Identifier: BUSL-1.1
set -eu

repository_root=$(cd -- "$(dirname -- "$0")/../.." && pwd)
image=${XENOTEER_NOVNC_SPIKE_IMAGE:-xenoteer:novnc-spike}
base_image=${XENOTEER_NOVNC_SPIKE_BASE_IMAGE:-xenoteer:phase0}
seccomp_profile=$repository_root/container/spikes/browser/seccomp_profile.json

for required in \
    container/spikes/novnc/Dockerfile \
    container/spikes/novnc/README.md \
    container/spikes/novnc/browser-proof.html \
    container/spikes/novnc/mandatory.json \
    container/spikes/novnc/packages.lock \
    container/spikes/novnc/critical-assets.sha256 \
    container/spikes/novnc/rfb_websocket_probe.py \
    container/spikes/novnc/run-spike.sh; do
    if [ ! -f "$repository_root/$required" ]; then
        echo "missing noVNC spike input: $required" >&2
        exit 1
    fi
done

if [ ! -f "$seccomp_profile" ]; then
    echo "missing pinned browser seccomp profile: $seccomp_profile" >&2
    exit 1
fi

command -v docker >/dev/null 2>&1 || {
    echo "docker is required for the noVNC spike" >&2
    exit 2
}
command -v rg >/dev/null 2>&1 || {
    echo "ripgrep is required for the noVNC static decision checks" >&2
    exit 2
}

remaining_plan_x11vnc=$(rg -l 'x11vnc' "$repository_root/plans" || true)
if [ "$remaining_plan_x11vnc" != "$repository_root/plans/06-observation-and-streaming.md" ]; then
    echo "stale selected x11vnc references remain in the plan corpus:" >&2
    printf '%s\n' "$remaining_plan_x11vnc" >&2
    exit 1
fi
if rg -l 'x11vnc' "$repository_root/container/spikes/novnc" \
    --glob '!README.md' >/dev/null; then
    echo "x11vnc remains in selected viewer spike implementation files" >&2
    exit 1
fi
if rg -n 'shm_size:[[:space:]]*2gb|--shm-size=2g|2 GiB recommended|2 GiB `/dev/shm`' \
    "$repository_root/plans" >/dev/null; then
    echo "stale two-GiB browser shared-memory setting remains in plans" >&2
    exit 1
fi
rg -F 'tigervnc-scraping-server=1.15.0+dfsg-2.1~deb13u1' \
    "$repository_root/container/spikes/novnc/Dockerfile" >/dev/null
for mandatory_flag in \
    '-AcceptKeyEvents=0' \
    '-AcceptPointerEvents=0' \
    '-AcceptSetDesktopSize=0' \
    '-AcceptCutText=0' \
    '-SendCutText=0'; do
    rg -F -- "$mandatory_flag" \
        "$repository_root/container/spikes/novnc/run-spike.sh" >/dev/null
done
forbidden_seccomp="seccomp=un"'confined'
if rg -F -- "--security-opt $forbidden_seccomp" "$0" >/dev/null; then
    echo "noVNC gate must not disable Docker seccomp" >&2
    exit 1
fi
rg -F -- '--shm-size 4g' "$0" >/dev/null

"$repository_root/scripts/container/test-browser-seccomp.sh"

docker build \
    --file "$repository_root/container/spikes/novnc/Dockerfile" \
    --build-arg "SPIKE_BASE_IMAGE=$base_image" \
    --tag "$image" \
    "$repository_root"

distributable=$(docker image inspect "$image" \
    --format '{{index .Config.Labels "com.aeor.xenoteer.distributable"}}')
if [ "$distributable" != "false" ]; then
    echo "noVNC spike image must be labelled non-distributable" >&2
    exit 1
fi

exposed_ports=$(docker image inspect "$image" --format '{{json .Config.ExposedPorts}}')
case "$exposed_ports" in
    *5900*|*6080*)
        echo "raw RFB or websockify port was exposed: $exposed_ports" >&2
        exit 1
        ;;
esac

docker run --rm \
    --network none \
    --security-opt "seccomp=$seccomp_profile" \
    --shm-size 4g \
    --pids-limit 256 \
    "$image"

echo "noVNC spike image gate passed with the pinned narrow browser seccomp profile: $image"
