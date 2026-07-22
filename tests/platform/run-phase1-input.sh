#!/bin/sh
set -eu

# This is the final Phase 1 live gate. It exercises the independent recorder,
# the fixture-only xdotool oracle, and the native actor; no missing lane is
# converted into a passing skip.

for required in Xvfb xauth xdpyinfo cargo flock seq mktemp rm tr touch jq \
    xdotool kill wait sleep cat tail timeout; do
    command -v "$required" >/dev/null 2>&1 || {
        echo "missing required command: $required" >&2
        exit 2
    }
done

test_dir=$(mktemp -d "${TMPDIR:-/tmp}/xenoteer-phase1-input.XXXXXX")
xvfb_pid=
recorder_pid=
display_lock_held=false
cleanup() {
    cleanup_status=$?
    if [ "$cleanup_status" -ne 0 ]; then
        echo "phase1 input harness failed; captured evidence follows" >&2
        for evidence_file in \
            "${actor_result_log:-}" "${actor_recorder_log:-}" \
            "${test_dir:-}/actor-recorder.err" "${test_dir:-}/xvfb.log"; do
            if [ -n "$evidence_file" ] && [ -s "$evidence_file" ]; then
                echo "evidence file: $evidence_file" >&2
                tail -n 200 "$evidence_file" >&2
            fi
        done
    fi
    if [ -n "$recorder_pid" ]; then
        kill "$recorder_pid" 2>/dev/null || true
        wait "$recorder_pid" 2>/dev/null || true
    fi
    if [ -n "$xvfb_pid" ]; then
        kill "$xvfb_pid" 2>/dev/null || true
        wait "$xvfb_pid" 2>/dev/null || true
    fi
    if [ "$display_lock_held" = true ]; then
        flock -u 9 2>/dev/null || true
        exec 9>&-
        display_lock_held=false
    fi
    rm -rf "$test_dir"
    return "$cleanup_status"
}
trap cleanup EXIT INT TERM

display_number=
for candidate in $(seq 240 299); do
    display_lock_file=/tmp/xenoteer-x-display-$candidate.lock
    exec 9>"$display_lock_file"
    if flock -n 9; then
        if [ ! -e "/tmp/.X11-unix/X$candidate" ]; then
            display_number=$candidate
            display_lock_held=true
            break
        fi
        flock -u 9
    fi
    exec 9>&-
done
if [ -z "$display_number" ]; then
    echo "no free isolated X display number in 240..299" >&2
    exit 2
fi

display=:$display_number
auth_file=$test_dir/Xauthority
cookie=$(tr -d '-' </proc/sys/kernel/random/uuid)
touch "$auth_file"
xauth -f "$auth_file" add "$display" . "$cookie"
Xvfb "$display" -screen 0 800x600x24 -dpi 96 -nolisten tcp -auth "$auth_file" \
    >"$test_dir/xvfb.log" 2>&1 &
xvfb_pid=$!

x_ready=false
attempt=0
while [ "$attempt" -lt 100 ]; do
    if DISPLAY=$display XAUTHORITY=$auth_file xdpyinfo >/dev/null 2>&1; then
        x_ready=true
        break
    fi
    kill -0 "$xvfb_pid" 2>/dev/null || {
        cat "$test_dir/xvfb.log" >&2
        exit 1
    }
    attempt=$((attempt + 1))
    sleep 0.02
done
if [ "$x_ready" != true ]; then
    echo "Xvfb failed protocol readiness" >&2
    cat "$test_dir/xvfb.log" >&2
    exit 1
fi

export DISPLAY="$display"
export XAUTHORITY="$auth_file"
cargo build -j 4 --manifest-path fixtures/x11/Cargo.toml --locked
recorder=fixtures/x11/target/debug/x11-event-recorder

wait_for_json() {
    jq_file=$1
    jq_expression=$2
    watched_pid=$3
    evidence_name=$4
    jq_ready=false
    jq_attempt=0
    while [ "$jq_attempt" -lt 150 ]; do
        if jq -s -e "$jq_expression" "$jq_file" >/dev/null 2>&1; then
            jq_ready=true
            break
        fi
        if [ -n "$watched_pid" ] && ! kill -0 "$watched_pid" 2>/dev/null; then
            break
        fi
        jq_attempt=$((jq_attempt + 1))
        sleep 0.02
    done
    if [ "$jq_ready" != true ]; then
        echo "timed out waiting for $evidence_name" >&2
        cat "$jq_file" >&2
        return 1
    fi
}

stop_recorder() {
    kill "$recorder_pid" 2>/dev/null || true
    wait "$recorder_pid" 2>/dev/null || true
    recorder_pid=
}

# xdotool is a fixture-only semantic oracle. Production crates never invoke it.
oracle_log=$test_dir/xdotool-oracle.jsonl
"$recorder" --focus-before-ready >"$oracle_log" 2>"$test_dir/oracle.err" &
recorder_pid=$!
wait_for_json "$oracle_log" \
    'any(.[]; .type == "ready_metadata" and .focus_requested == true and .observed_focus == .window and .painted == true)' \
    "$recorder_pid" "focused recorder readiness"
oracle_window=$(jq -sr 'map(select(.type == "ready"))[0].window' "$oracle_log")

xdotool mousemove --sync 80 90
xdotool mousedown 1
xdotool mouseup 1
xdotool keydown a
xdotool keyup a
wait_for_json "$oracle_log" \
    'any(.[]; .type == "motion" and .root_x == 80 and .root_y == 90 and (.time | type) == "number" and (.state | type) == "number") and any(.[]; .type == "button_press" and .detail == 1 and (.time | type) == "number" and (.state | type) == "number") and any(.[]; .type == "button_release" and .detail == 1 and (.time | type) == "number" and (.state | type) == "number") and any(.[]; .type == "key_press" and .keysym == 97 and (.time | type) == "number" and (.state | type) == "number") and any(.[]; .type == "key_release" and .keysym == 97 and (.time | type) == "number" and (.state | type) == "number")' \
    "$recorder_pid" "xdotool motion/button/key oracle evidence"
jq -s -e --argjson window "$oracle_window" '
    all(.[] | select(.type == "motion" or .type == "button_press" or
        .type == "button_release" or .type == "key_press" or
        .type == "key_release"); .window == $window) and
    ([.[] | .type] | index("motion") < index("button_press") and
        index("button_press") < index("button_release") and
        index("button_release") < index("key_press") and
        index("key_press") < index("key_release"))
' "$oracle_log" >/dev/null
stop_recorder

# Prove application-style pointer interference is one-shot and barrier-observed.
warp_log=$test_dir/post-motion-warp.jsonl
"$recorder" --focus-before-ready --post-motion-warp 300 310 \
    >"$warp_log" 2>"$test_dir/warp.err" &
recorder_pid=$!
wait_for_json "$warp_log" \
    'any(.[]; .type == "ready_metadata" and .post_motion_warp == {"x":300,"y":310})' \
    "$recorder_pid" "post-motion warp configuration"
# Do not use xdotool's endpoint wait here: the recorder intentionally undoes
# the position immediately. The flushed JSON and QueryPointer-backed
# pointer_warped record below are the synchronization and effect proof.
xdotool mousemove 40 50
wait_for_json "$warp_log" \
    'any(.[]; .type == "motion" and .root_x == 40 and .root_y == 50) and any(.[]; .type == "pointer_warped" and .requested_root_x == 300 and .requested_root_y == 310 and .observed_root_x == 300 and .observed_root_y == 310)' \
    "$recorder_pid" "barrier-observed post-motion warp"
stop_recorder

# Prove grab/release and target-destruction failure controls are explicit.
destroy_log=$test_dir/grab-destroy.jsonl
"$recorder" --focus-before-ready --grab-pointer \
    --release-pointer-grab-after-button-press --destroy-after-button-press \
    >"$destroy_log" 2>"$test_dir/destroy.err" &
recorder_pid=$!
wait_for_json "$destroy_log" \
    'any(.[]; .type == "ready_metadata" and .pointer_grab_requested == true and .pointer_grabbed == true)' \
    "$recorder_pid" "active pointer grab readiness"
xdotool mousemove --sync 120 130
xdotool mousedown 1
wait_for_json "$destroy_log" \
    'any(.[]; .type == "button_press" and .detail == 1) and any(.[]; .type == "pointer_ungrabbed" and .reason == "button_press") and any(.[]; .type == "destroy_requested") and any(.[]; .type == "destroy")' \
    "$recorder_pid" "grab release and target destruction evidence"
wait "$recorder_pid"
recorder_pid=
xdotool mouseup 1

# The observed-event limit bounds the recorder independently of its controller.
limit_log=$test_dir/event-limit.jsonl
"$recorder" --max-events 1 >"$limit_log" 2>"$test_dir/limit.err" &
recorder_pid=$!
wait_for_json "$limit_log" 'any(.[]; .type == "ready_metadata" and .max_events == 1)' \
    "$recorder_pid" "event limit readiness metadata"
xdotool mousemove --sync 200 210
wait "$recorder_pid"
recorder_pid=
jq -s -e '
    [.[] | select(.type != "ready" and .type != "ready_metadata")] | length == 1
' "$limit_log" >/dev/null

actor_source=crates/xenoteer-x11/examples/phase1-input.rs
if [ ! -f "$actor_source" ]; then
    echo "phase1 actor integration source is missing: $actor_source" >&2
    exit 2
fi

actor_recorder_log=$test_dir/actor-recorder.jsonl
actor_result_log=$test_dir/actor-result.jsonl
"$recorder" --focus-before-ready >"$actor_recorder_log" \
    2>"$test_dir/actor-recorder.err" &
recorder_pid=$!
wait_for_json "$actor_recorder_log" \
    'any(.[]; .type == "ready_metadata" and .observed_focus == .window)' \
    "$recorder_pid" "actor recorder readiness"
actor_window=$(jq -sr 'map(select(.type == "ready"))[0].window' "$actor_recorder_log")

timeout 180s cargo run --quiet -j 4 -p xenoteer-x11 --example phase1-input \
    --features native-xkbcommon -- \
    --window "$actor_window" --scenario conformance >"$actor_result_log"
jq -e . "$actor_result_log" >/dev/null
temporary_keycode=$(jq -sr \
    'map(select(.type == "temporary_mapping_proof"))[0].keycode' \
    "$actor_result_log")

jq -s -e --argjson window "$actor_window" '
    ([.[] | select(.type == "action") | .name][0:7] == [
        "interpolated_move", "instant_move", "double_click", "drag",
        "scroll_down", "scroll_right", "delayed_click"
    ]) and
    any(.[];
        .type == "keyboard_prime" and .keycode >= 8 and
        .events_emitted == 2 and
        ((.result == "completed" and .cleanup_succeeded == null) or
         (.result == "mapping_changed_after_effect" and
          .cleanup_succeeded == true))
    ) and
    all(.[] | select(.type == "action"); .result == "completed") and
    all(.[] | select(
        .type == "action" and
        (.name == "interpolated_move" or .name == "instant_move" or
         .name == "double_click" or .name == "drag")
    ); .requested_pointer == .observed_pointer) and
    any(.[];
        .type == "independent_observation" and
        .while_action_pending == true and .elapsed_ms <= 250
    ) and
    any(.[];
        .type == "cancellation_boundary" and
        .result == "cancelled_after_effect" and
        .events_emitted > 0
    ) and
    any(.[];
        .type == "complete" and .scenario == "conformance" and
        .window == $window and .actor_exit == "stopped" and
        .pointer_actions == 8 and .keyboard_actions == 6
    )
' "$actor_result_log" >/dev/null

wait_for_json "$actor_recorder_log" '
    ([.[] | select(.type == "button_press" and .detail == 1)] | length) >= 5 and
    any(.[]; .type == "motion" and .root_x == 390 and .root_y == 300)
' "$recorder_pid" "complete pointer actor evidence"

# Event ordering and X server timestamps are checked independently of the
# actor result stream. The fixed coordinates divide the conformance scenario
# into unambiguous motion/click/drag segments.
jq -s -e '
    def delta($before; $after):
        (($after - $before + 4294967296) % 4294967296);
    . as $events |
    ([range(0; length) as $i |
        select(.[$i].type == "motion" and
               .[$i].root_x == 160 and .[$i].root_y == 120) | $i][0]) as $smooth_end |
    ([range(0; length) as $i |
        select(.[$i].type == "motion" and
               .[$i].root_x == 190 and .[$i].root_y == 150) | $i][0]) as $instant_end |
    ([range(0; length) as $i |
        select(.[$i].type == "motion" and
               .[$i].root_x == 220 and .[$i].root_y == 180) | $i][0]) as $click_end |
    (.[0:($smooth_end + 1)] | map(select(.type == "motion"))) as $smooth |
    (.[$smooth_end + 1:($instant_end + 1)] | map(select(.type == "motion"))) as $instant |
    ([range(0; length) as $i |
        select(.[$i].type == "button_press" and .[$i].detail == 1) | $i]) as $press |
    ([range(0; length) as $i |
        select(.[$i].type == "button_release" and .[$i].detail == 1) | $i]) as $release |
    ([range(0; length) as $i |
        select((.[$i].type == "button_press" or .[$i].type == "button_release") and
               .[$i].detail == 1) | $i]) as $button1 |
    (.[$button1[4] + 1:$button1[5]] | map(select(.type == "motion"))) as $drag |
    (.[$button1[8] + 1:$button1[9]] | map(select(.type == "motion"))) as $cancelled_drag |

    ($smooth | length) >= 8 and
    ($smooth[-1] | .root_x == 160 and .root_y == 120) and
    any($smooth[]; .root_x != 160 and .root_y != 120) and
    all(range(1; $smooth | length);
        $smooth[.].root_x <= $smooth[. - 1].root_x and
        $smooth[.].root_y <= $smooth[. - 1].root_y) and
    delta($smooth[0].time; $smooth[-1].time) >= 180 and
    delta($smooth[0].time; $smooth[-1].time) <= 350 and
    ($instant | length) == 1 and
    ($instant[0] | .root_x == 190 and .root_y == 150) and
    $smooth_end < $instant_end and $instant_end < $click_end and

    ($button1 | length) == 10 and
    ([$events[$button1[0]].type, $events[$button1[1]].type,
      $events[$button1[2]].type, $events[$button1[3]].type] ==
      ["button_press", "button_release", "button_press", "button_release"]) and
    all(range(0; 4);
        $events[$button1[.]].root_x == 220 and
        $events[$button1[.]].root_y == 180) and
    delta($events[$click_end].time; $events[$button1[0]].time) >= 25 and
    delta($events[$button1[0]].time; $events[$button1[1]].time) >= 40 and
    delta($events[$button1[1]].time; $events[$button1[2]].time) >= 90 and
    delta($events[$button1[2]].time; $events[$button1[3]].time) >= 40 and

    ($drag | length) >= 8 and
    ($drag[0] | .root_x == 220 and .root_y == 180) and
    ($drag[-1] | .root_x == 340 and .root_y == 260) and
    any($drag[]; .root_x == 260 and .root_y == 190) and
    any($drag[]; .root_x == 300 and .root_y == 230) and
    all($drag[]; ((.state / 256 | floor) % 2) == 1) and
    ($events[$button1[5]] |
        .type == "button_release" and .root_x == 340 and .root_y == 260 and
        ((.state / 256 | floor) % 2) == 1) and

    ($cancelled_drag | length) >= 2 and
    ($cancelled_drag[0] | .root_x == 340 and .root_y == 260) and
    ($cancelled_drag[-1] | .root_x == 390 and .root_y == 300) and
    all($cancelled_drag[]; ((.state / 256 | floor) % 2) == 1) and
    ($events[$button1[9]] |
        .type == "button_release" and .root_x == 390 and .root_y == 300 and
        ((.state / 256 | floor) % 2) == 1) and

    ([.[] | select(.type == "button_press" and .detail == 5)] | length) == 2 and
    ([.[] | select(.type == "button_release" and .detail == 5)] | length) == 2 and
    ([.[] | select(.type == "button_press" and .detail == 7)] | length) == 2 and
    ([.[] | select(.type == "button_release" and .detail == 7)] | length) == 2 and
    ($press | length) == 5 and ($release | length) == 5
' "$actor_recorder_log" >/dev/null
wait_for_json "$actor_recorder_log" '
    ([.[] | select(
        (.type == "key_press" or .type == "key_release") and
        .keysym == 16786947
    )] | length) == 2 and
    ([.[] | select(
        .type == "mapping_notify" and .request == 1 and .count == 1
    )] | length) >= 2
' "$recorder_pid" "temporary mapping install, key, and restore evidence"
stop_recorder

keyboard_actions=$(jq -sr 'map(select(.type == "complete"))[0].keyboard_actions' \
    "$actor_result_log")
if [ "$keyboard_actions" -ne 6 ]; then
    echo "unexpected completed keyboard action count: $keyboard_actions" >&2
    exit 1
fi

jq -s -e '
    ([.[] | select(.type == "action") | .name][7:] == [
        "named_enter", "scalar_x", "chord_control_a", "keyboard_sequence",
        "physical_text_current_layout", "physical_text_extended_temporary"
    ]) and
    any(.[]; .type == "action" and .name == "named_enter" and
        .result == "completed" and .keyboard_bindings > 0 and
        .effect_evidence == "journal") and
    all(.[] | select(.type == "action" and
        (.name == "scalar_x" or .name == "chord_control_a" or
         .name == "keyboard_sequence" or
         .name == "physical_text_current_layout" or
         .name == "physical_text_extended_temporary"));
        .result == "completed" and .keyboard_bindings == 0 and
        .effect_evidence == "redacted_keyboard" and
        .effect_provisional == 0 and .effect_confirmed == .events_emitted) and
    any(.[]; .type == "action" and .name == "physical_text_current_layout" and
        .result == "completed" and .text_scalar_count == 4 and
        .requested_text_mode == "current_layout" and
        .current_layout_scalars == 4 and .temporary_mapping_scalars == 0 and
        .temporary_mappings_installed == 0 and
        .temporary_mappings_restored == 0 and
        .temporary_mapping_restoration_proven == null) and
    any(.[]; .type == "action" and
        .name == "physical_text_extended_temporary" and
        .result == "completed" and .events_emitted == 2 and
        .completed_units == 1 and .keyboard_bindings == 0 and
        .text_scalar_count == 1 and
        .requested_text_mode == "extended_temporary_mapping" and
        .current_layout_scalars == 0 and .temporary_mapping_scalars == 1 and
        .temporary_mappings_installed == 1 and
        .temporary_mappings_restored == 1 and
        .temporary_mapping_restoration_proven == true and
        .effect_evidence == "redacted_keyboard" and
        .effect_provisional == 0 and .effect_confirmed == 2) and
    any(.[]; .type == "temporary_mapping_proof" and
        .keycode >= 8 and .keysyms_per_keycode > 0 and
        .mapping_word_count == .keysyms_per_keycode and
        .before_all_no_symbol == true and .before_unpressed == true and
        .before_nonmodifier == true and .after_exact_match == true and
        .after_unpressed == true and .after_nonmodifier == true)
' "$actor_result_log" >/dev/null

# This event sequence proves named/scalar resolution, modifier-first and
# reverse-release chord behavior, sequence boundaries, and current-layout
# physical text. It also independently brackets extended text with the core
# mapping install and restore notifications. Shifted text deliberately reuses
# the same raw keycode as its unshifted symbol; the recorded state mask is the
# semantic difference.
jq -s -e --argjson temporary_keycode "$temporary_keycode" '
    def delta($before; $after):
        (($after - $before + 4294967296) % 4294967296);
    . as $events |
    ([.[] | select(.type == "key_press" or .type == "key_release")]) as $all_keys |
    ($all_keys | map(select(.keysym == 0))) as $warmup |
    ($all_keys | map(select(.keysym != 0 and .keysym != 16786947))) as $keys |
    ($all_keys | map(select(.keysym == 16786947))) as $temporary_keys |
    ([range(0; length) as $i |
        select(.[$i].type == "mapping_notify" and .[$i].request == 1 and
               .[$i].first_keycode == $temporary_keycode and .[$i].count == 1) |
        $i]) as $temporary_mapping |
    ([range(0; length) as $i |
        select((.[$i].type == "key_press" or .[$i].type == "key_release") and
               .[$i].keysym != 0 and .[$i].keysym != 16786947) | $i]) as $key_indices |
    ([range(0; length) as $i |
        select((.[$i].type == "key_press" or .[$i].type == "key_release") and
               .[$i].keysym == 16786947) | $i]) as $temporary_key_indices |
    (($warmup | length) == 0 or
     (($warmup | length) == 2 and
      $all_keys[0].keysym == 0 and $all_keys[1].keysym == 0 and
      $warmup[0].type == "key_press" and $warmup[1].type == "key_release" and
      $warmup[0].keycode == $warmup[1].keycode and
      $warmup[0].state == 0 and $warmup[1].state == 0 and
      any(.[]; .type == "mapping_notify" and .request == 1 and
          .first_keycode == 8 and .count == 248) and
      any(.[]; .type == "mapping_notify" and .request == 0))) and
    ($keys | map([.type, .keysym])) == [
        ["key_press",65293], ["key_release",65293],
        ["key_press",120], ["key_release",120],
        ["key_press",65507], ["key_press",97],
        ["key_release",97], ["key_release",65507],
        ["key_press",65307], ["key_release",65307],
        ["key_press",98], ["key_release",98],
        ["key_press",65505], ["key_press",99],
        ["key_release",99], ["key_release",65505],
        ["key_press",65505], ["key_press",97],
        ["key_release",97], ["key_release",65505],
        ["key_press",122], ["key_release",122],
        ["key_press",49], ["key_release",49],
        ["key_press",65505], ["key_press",49],
        ["key_release",49], ["key_release",65505]
    ] and
    ($temporary_keys | map([.type, .keysym, .keycode])) == [
        ["key_press",16786947,$temporary_keycode],
        ["key_release",16786947,$temporary_keycode]
    ] and
    ($temporary_mapping | length) == 2 and
    ($key_indices | length) == 28 and ($temporary_key_indices | length) == 2 and
    $key_indices[-1] < $temporary_mapping[0] and
    $temporary_mapping[0] < $temporary_key_indices[0] and
    $temporary_key_indices[0] < $temporary_key_indices[1] and
    $temporary_key_indices[1] < $temporary_mapping[1] and
    all(range(0; $keys | length); ($keys[.].keycode | type) == "number") and
    $keys[0].keycode == $keys[1].keycode and
    $keys[2].keycode == $keys[3].keycode and
    $keys[4].keycode == $keys[7].keycode and
    $keys[5].keycode == $keys[6].keycode and
    $keys[12].keycode == $keys[15].keycode and
    $keys[13].keycode == $keys[14].keycode and
    $keys[16].keycode == $keys[19].keycode and
    $keys[17].keycode == $keys[18].keycode and
    $keys[17].keycode == $keys[5].keycode and
    $keys[22].keycode == $keys[23].keycode and
    $keys[25].keycode == $keys[26].keycode and
    $keys[25].keycode == $keys[22].keycode and
    ($keys[5].state % 8) >= 4 and ($keys[6].state % 8) >= 4 and
    ($keys[13].state % 2) == 1 and ($keys[14].state % 2) == 1 and
    ($keys[17].state % 2) == 1 and ($keys[18].state % 2) == 1 and
    ($keys[25].state % 2) == 1 and ($keys[26].state % 2) == 1 and
    delta($keys[0].time; $keys[1].time) >= 30 and
    delta($keys[2].time; $keys[3].time) >= 30 and
    delta($keys[5].time; $keys[6].time) >= 40 and
    delta($keys[9].time; $keys[10].time) >= 45 and
    delta($keys[11].time; $keys[12].time) >= 45 and
    delta($keys[19].time; $keys[20].time) >= 15 and
    delta($keys[21].time; $keys[22].time) >= 15 and
    delta($keys[23].time; $keys[24].time) >= 15
' "$actor_recorder_log" >/dev/null

echo "Phase 1 actor integration evidence passed"
