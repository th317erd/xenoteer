#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
container=${1:?usage: test-viewer-denial.sh CONTAINER}
fixture_dir=/run/xenoteer/viewer-denial
events=/run/user/1000/viewer-denial-events.jsonl
event_errors=/run/user/1000/viewer-denial-events.err
selections=/run/user/1000/viewer-denial-selections.jsonl
selection_errors=/run/user/1000/viewer-denial-selections.err
clipboard_canary_file=$fixture_dir/server-clipboard-canary
rfb_ready=$fixture_dir/rfb-client-ready
rfb_continue=$fixture_dir/rfb-client-continue
rfb_resize_ready=$fixture_dir/rfb-resize-processed.json
rfb_resize_continue=$fixture_dir/rfb-resize-continue
rfb_output=$fixture_dir/rfb-client-result.json
rfb_errors=$fixture_dir/rfb-client.err

cleanup() {
  docker exec "$container" sh -c '
    pkill -TERM -u 1000 -f "^/run/xenoteer/viewer-denial/x11-event-recorder --focus-before-ready$" 2>/dev/null || true
    pkill -TERM -u 1000 -f "^/run/xenoteer/viewer-denial/x11-selection-sentinel --canary-file /run/xenoteer/viewer-denial/server-clipboard-canary$" 2>/dev/null || true
    pkill -TERM -f "^python3 /run/xenoteer/viewer-denial/rfb_websocket_probe.py rfb " 2>/dev/null || true
  ' >/dev/null 2>&1 || true
}
trap cleanup EXIT

cargo_args=(
  build --quiet --release --locked --jobs 4
  --manifest-path "$repo_root/fixtures/x11/Cargo.toml"
  --bin x11-event-recorder
  --bin x11-input-driver
  --bin x11-selection-sentinel
)
if command -v cargo >/dev/null 2>&1; then
  cargo "${cargo_args[@]}"
elif [[ -n ${SUDO_UID:-} && $SUDO_UID != 0 ]]; then
  invoking_home=$(getent passwd "$SUDO_UID" | cut -d: -f6)
  invoking_cargo="$invoking_home/.cargo/bin/cargo"
  if [[ ! -x $invoking_cargo ]]; then
    printf 'cargo is unavailable for invoking UID %s\n' "$SUDO_UID" >&2
    exit 77
  fi
  sudo -H -u "#$SUDO_UID" "$invoking_cargo" "${cargo_args[@]}"
else
  printf 'cargo is required to build the X11 denial fixtures\n' >&2
  exit 77
fi

docker exec "$container" install -d -m 0755 "$fixture_dir"
for binary in x11-event-recorder x11-input-driver x11-selection-sentinel; do
  docker cp "$repo_root/fixtures/x11/target/release/$binary" \
    "$container:$fixture_dir/$binary" >/dev/null
done
docker cp "$repo_root/container/spikes/novnc/rfb_websocket_probe.py" \
  "$container:$fixture_dir/rfb_websocket_probe.py" >/dev/null
docker exec "$container" chmod 0555 \
  "$fixture_dir/x11-event-recorder" \
  "$fixture_dir/x11-input-driver" \
  "$fixture_dir/x11-selection-sentinel" \
  "$fixture_dir/rfb_websocket_probe.py"
docker exec "$container" rm -f \
  "$events" "$event_errors" "$selections" "$selection_errors" \
  "$rfb_ready" "$rfb_continue" "$rfb_resize_ready" "$rfb_resize_continue" \
  "$rfb_output" "$rfb_errors" \
  "$clipboard_canary_file"

clipboard_canary="xenoteer-viewer-egress-secret-$(tr -d '-' </proc/sys/kernel/random/uuid)"
canary_sha256=$(printf '%s' "$clipboard_canary" | sha256sum | awk '{print $1}')
docker exec --env "CLIPBOARD_CANARY=$clipboard_canary" "$container" sh -eu -c '
  umask 077
  printf %s "$CLIPBOARD_CANARY" >"$1"
  chown 1000:1000 "$1"
' sh "$clipboard_canary_file"

# Start far from the recorder so mapping it cannot itself create a motion
# observation at the coordinate used by the hostile RFB pointer message.
docker exec "$container" /command/s6-envdir /run/xenoteer/env /command/s6-setuidgid xenoteer \
  "$fixture_dir/x11-input-driver" --x 1800 --y 1000 --expected-window 0 \
  --skip-window-check >/dev/null

docker exec --detach "$container" /command/s6-envdir /run/xenoteer/env \
  /command/s6-setuidgid xenoteer \
  sh -c "exec $fixture_dir/x11-event-recorder --focus-before-ready >$events 2>$event_errors"

for _ in {1..100}; do
  if docker exec "$container" grep -Fq '"type":"ready"' "$events" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
docker exec "$container" grep -Fq '"type":"ready"' "$events"

recorder_window=$(docker exec "$container" cat "$events" \
  | jq -er 'select(.type == "ready") | .window' | head -n 1)
[[ $recorder_window =~ ^[0-9]+$ ]]
docker exec "$container" sh -eu -c '
  pgrep -u 1000 -f "^/run/xenoteer/viewer-denial/x11-event-recorder --focus-before-ready$" >/dev/null
'
docker exec "$container" cat "$events" | jq -se --argjson window "$recorder_window" '
  any(.[]; .type == "ready_metadata"
    and .window == $window
    and .focus_requested == true
    and .observed_focus == $window)
' >/dev/null

# Exercise the production websockify and X0tigervnc processes. Hold a fully
# negotiated RFB client open, then acquire both selections so any forbidden
# server-to-viewer clipboard synchronization must occur on that live client.
docker exec "$container" /usr/local/libexec/xenoteer/probe-viewer-protocol
docker exec --detach "$container" sh -c '
  exec env PYTHONDONTWRITEBYTECODE=1 python3 "$1" rfb \
    --width 1920 --height 1080 --skip-framebuffer-proof \
    --ready-file "$2" --continue-file "$3" --observe-seconds 1 \
    --forbidden-server-bytes-file "$6" \
    --resize-ready-file "$7" --resize-continue-file "$8" >"$4" 2>"$5"
' sh "$fixture_dir/rfb_websocket_probe.py" "$rfb_ready" "$rfb_continue" \
  "$rfb_output" "$rfb_errors" "$clipboard_canary_file" \
  "$rfb_resize_ready" "$rfb_resize_continue"
for _ in {1..100}; do
  docker exec "$container" test -s "$rfb_ready" 2>/dev/null && break
  sleep 0.1
done
docker exec "$container" grep -Fxq 'rfb_client_ready' "$rfb_ready"

docker exec --detach "$container" /command/s6-envdir /run/xenoteer/env \
  /command/s6-setuidgid xenoteer \
  sh -c "exec $fixture_dir/x11-selection-sentinel --canary-file $clipboard_canary_file >$selections 2>$selection_errors"
for _ in {1..100}; do
  docker exec "$container" grep -Fq '"type":"ready"' "$selections" 2>/dev/null && break
  sleep 0.1
done
docker exec "$container" grep -Fq '"type":"ready"' "$selections"
docker exec "$container" sh -eu -c '
  pgrep -u 1000 -f "^/run/xenoteer/viewer-denial/x11-selection-sentinel --canary-file /run/xenoteer/viewer-denial/server-clipboard-canary$" >/dev/null
'
docker exec "$container" touch "$rfb_continue"
for _ in {1..150}; do
  docker exec "$container" test -s "$rfb_resize_ready" 2>/dev/null && break
  sleep 0.1
done
if ! docker exec "$container" test -s "$rfb_resize_ready"; then
  docker exec "$container" cat "$rfb_errors" >&2 || true
  printf 'production RFB resize request produced no ordered protocol response\n' >&2
  exit 1
fi
resize_evidence=$(docker exec "$container" cat "$rfb_resize_ready")
jq -e '
  .ordered_protocol_barrier == "extended_desktop_size"
  and .reason == "client" and .reason_code == 1
  and .result == "prohibited" and .result_code == 1
  and .requested_geometry == "1024x768"
  and .server_init_geometry == "1920x1080"
  and .response_geometry == .server_init_geometry
  and any(.screens[];
    .x == 0 and .y == 0 and .width == 1920 and .height == 1080)
' <<<"$resize_evidence" >/dev/null
docker exec "$container" sh -eu -c '
  pgrep -f "^python3 /run/xenoteer/viewer-denial/rfb_websocket_probe.py rfb " >/dev/null
'

# The pointer must remain at the pre-negotiation coordinate, the focused
# recorder must see no key/pointer/button effects, and both X11 selections must
# remain owned by the sentinel. These checks run while the RFB client is held at
# the explicit post-SetDesktopSize rejection barrier, so geometry cannot race
# ahead of server-side request processing.
docker exec "$container" /command/s6-envdir /run/xenoteer/env \
  /command/s6-setuidgid xenoteer \
  "$fixture_dir/x11-input-driver" --query-only --x 1800 --y 1000 \
  --expected-window 0 --skip-window-check \
  --expected-focus-window "$recorder_window" >/dev/null
docker exec "$container" /command/s6-envdir /run/xenoteer/env \
  /command/s6-setuidgid xenoteer xdpyinfo \
  | grep -F 'dimensions:    1920x1080 pixels' >/dev/null
if docker exec "$container" grep -Eq \
  '"type":"(motion|button_press|button_release|key_press|key_release)"' "$events"; then
  printf 'production viewer changed focused X11 input state\n' >&2
  docker exec "$container" cat "$events" >&2
  exit 1
fi
docker exec "$container" sh -eu -c '
  pgrep -u 1000 -f "^/run/xenoteer/viewer-denial/x11-selection-sentinel --canary-file /run/xenoteer/viewer-denial/server-clipboard-canary$" >/dev/null
  ! grep -Fq "\"type\":\"selection_clear\"" /run/user/1000/viewer-denial-selections.jsonl
'
selection_evidence=$(docker exec "$container" cat "$selections")
targets_atom=$(jq -er 'select(.type == "ready") | .targets' <<<"$selection_evidence")
jq -se --argjson canary_bytes "${#clipboard_canary}" '
  any(.[]; .type == "ready" and .canary_bytes == $canary_bytes)
  and all(.[]; if .type == "selection_request" then .served_canary == false else true end)
' <<<"$selection_evidence" >/dev/null

# Distinguish TigerVNC's capability-only lookup from xfsettingsd's independent
# TARGETS lookup. With SendCutText=0, TigerVNC must never follow TARGETS with a
# request for UTF8_STRING, TEXT, or STRING, so the secret remains unread as well
# as absent from the RFB protocol.
tiger_requestor=
xfsettings_requestor=
while IFS= read -r requestor; do
  requestor_info=$(docker exec "$container" /command/s6-envdir /run/xenoteer/env \
    /command/s6-setuidgid xenoteer xwininfo -id "$requestor")
  case "$requestor_info" in
    *'"TigerVNC Clipboard (x0vncserver)"'*) tiger_requestor=$requestor ;;
    *'"xfsettingsd"'*) xfsettings_requestor=$requestor ;;
  esac
done < <(jq -r --argjson targets "$targets_atom" '
  select(.type == "selection_request" and .target == $targets) | .requestor
' <<<"$selection_evidence" | sort -u)
test -n "$tiger_requestor"
test -n "$xfsettings_requestor"
jq -se \
  --argjson targets "$targets_atom" \
  --argjson tiger "$tiger_requestor" \
  --argjson xfsettings "$xfsettings_requestor" '
  any(.[]; .type == "selection_request"
    and .requestor == $tiger and .target == $targets
    and .response_property != 0 and .served_canary == false)
  and any(.[]; .type == "selection_request"
    and .requestor == $xfsettings and .target == $targets
    and .response_property != 0 and .served_canary == false)
  and all(.[];
    if .type == "selection_request" and .requestor == $tiger
    then .target == $targets and .served_canary == false
    else true
    end)
' <<<"$selection_evidence" >/dev/null

docker exec "$container" touch "$rfb_resize_continue"
for _ in {1..100}; do
  docker exec "$container" test -s "$rfb_output" 2>/dev/null && break
  sleep 0.1
done
if ! docker exec "$container" test -s "$rfb_output"; then
  docker exec "$container" cat "$rfb_errors" >&2 || true
  printf 'production RFB denial probe did not complete after resize barrier\n' >&2
  exit 1
fi
docker exec "$container" cat "$rfb_output" | jq -e \
  --arg canary_sha256 "$canary_sha256" \
  --argjson resize "$resize_evidence" '
  (.sent_input_attempts | sort)
    == (["client_cut_text", "key", "pointer", "set_desktop_size"] | sort)
  and .server_cut_text_observation_seconds >= 1
  and .server_cut_text_messages == 0
  and .forbidden_server_bytes_seen == false
  and .forbidden_server_bytes_sha256 == $canary_sha256
  and .resize_rejection == $resize
' >/dev/null

# Positive XTEST control: move into the recorder and require a resulting
# MotionNotify. This proves the recorder and its log were capable of observing
# an input effect during the negative assertion above.
window_info=$(docker exec "$container" /command/s6-envdir /run/xenoteer/env \
  xwininfo -id "$recorder_window")
window_x=$(awk '/Absolute upper-left X:/ { print $4 }' <<<"$window_info")
window_y=$(awk '/Absolute upper-left Y:/ { print $4 }' <<<"$window_info")
[[ $window_x =~ ^-?[0-9]+$ && $window_y =~ ^-?[0-9]+$ ]]
control_x=$((window_x + 20))
control_y=$((window_y + 20))
docker exec "$container" /command/s6-envdir /run/xenoteer/env \
  /command/s6-setuidgid xenoteer \
  "$fixture_dir/x11-input-driver" --x "$control_x" --y "$control_y" \
  --expected-window "$recorder_window" --skip-window-check \
  --expected-focus-window "$recorder_window" --keycode 38 >/dev/null
for _ in {1..30}; do
  docker exec "$container" grep -Fq '"type":"motion"' "$events" 2>/dev/null && break
  sleep 0.1
done
docker exec "$container" grep -Fq '"type":"motion"' "$events"
docker exec "$container" grep -Fq '"type":"key_press"' "$events"
docker exec "$container" grep -Fq '"type":"key_release"' "$events"

printf 'production viewer input and clipboard denial verified: %s\n' "$container"
