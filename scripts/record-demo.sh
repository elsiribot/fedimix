#!/usr/bin/env bash
# Records the Phase 3 demo story headlessly on a local REGTEST devimint
# environment (not signet): its own headless sway at 1280x800, the real
# `fedimix` driven by scripts/demo-story/story.drive, wf-recorder for the
# video. The story: first launch and recovery phrase, join, Lightning receive,
# Saving on "This computer", a Saving receive, a Saving send whose change goes
# to Spending, one refill, history. Waits on blocks and the federation are cut
# out of the video. Afterwards three dark-theme screenshots of the same wallet.
#
# usage: scripts/record-demo.sh   (or: just record-demo)
# Output (FEDIMIX_DEMO_OUT overrides the folder):
#   /home/user/fedimix-demo/phase3/fedimix-demo.mp4
#   /home/user/fedimix-demo/phase3/screenshots/NN-name.png
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mode=record
# shellcheck source=demo-lib.sh
source "$root/scripts/demo-lib.sh"

out_dir="${FEDIMIX_DEMO_OUT:-$HOME/fedimix-demo/phase3}"
shots="$out_dir/screenshots"
video="$out_dir/fedimix-demo.mp4"
size=1280x800

# ---- Inside `devimint dev-fed --exec` ----

rec_pid=""
segments=0
output=""

cleanup() {
    [[ -n "$rec_pid" ]] && kill -INT "$rec_pid" 2>/dev/null || true
    [[ -n "${demo_flow_pid:-}" ]] && kill "$demo_flow_pid" 2>/dev/null || true
    [[ -n "$demo_miner_pid" ]] && kill "$demo_miner_pid" 2>/dev/null || true
    [[ -n "$demo_sway_pid" ]] && kill "$demo_sway_pid" 2>/dev/null || true
    rm -f "$demo_pid_file"
}

rec_start() {
    [[ -z "$rec_pid" ]] || return 0
    segments=$((segments + 1))
    local file
    file=$(printf '%s/seg-%02d.mp4' "$demo_work" "$segments")
    # -D: every frame, so a still screen keeps its time in the video.
    wf-recorder -D -o "$output" -f "$file" -c libx264 -x yuv420p -r 30 \
        </dev/null >"$demo_work/wf-recorder-$segments.log" 2>&1 &
    rec_pid=$!
    sleep 1
}

rec_stop() {
    [[ -n "$rec_pid" ]] || return 0
    kill -INT "$rec_pid" 2>/dev/null || true
    wait "$rec_pid" 2>/dev/null || true
    rec_pid=""
}

# Payments run in the background: the app waits at most 10 s for a shot's answer.
demo_on_shot() {
    local shot="$1"
    case "$shot" in
        _rec) rec_start; return ;;
        _cut) rec_stop; return ;;
    esac
    demo_grim "$shots/$shot.png"
    say "screenshot $shot.png"
    case "$shot" in
        06-invoice)
            (demo_pay_invoice "$(cat "$demo_work/invoice.txt")" | while read -r l; do say "$l"; done) &
            ;;
        11-saving-address)
            (demo_send_onchain "$(cat "$demo_work/saving-address.txt")" 0.01 >/dev/null) &
            ;;
        17-saving-address-2)
            (demo_send_onchain "$(cat "$demo_work/saving-address-2.txt")" 0.005 >/dev/null) &
            ;;
    esac
}

join_segments() {
    local list="$demo_work/segments.txt" seg
    : > "$list"
    for seg in "$demo_work"/seg-*.mp4; do
        [[ -s "$seg" ]] && echo "file '$seg'" >> "$list"
    done
    [[ -s "$list" ]] || die "no video segments were recorded"
    ffmpeg -loglevel error -y -f concat -safe 0 -i "$list" \
        -c:v libx264 -pix_fmt yuv420p -r 30 -movflags +faststart "$video"
    local secs
    secs=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$video")
    say "video: $video (${secs%.*} s, $segments segments)"
}

inner() {
    trap cleanup EXIT
    trap 'exit 130' INT TERM
    echo $$ > "$demo_pid_file"
    export FEDIMIX_CHAIN_URL="http://127.0.0.1:$FM_PORT_ESPLORA"
    demo_write_env
    echo "$started_marker"
    say "regtest federation up; recording the story (several minutes, most of it cut)..."
    demo_start_sway "$demo_work/sway" "$size"
    output=$(demo_output_name)
    say "recording sway output $output at $size"
    demo_start_miner 2
    export FEDIMIX_SAVING_DEST
    FEDIMIX_SAVING_DEST=$(btc getnewaddress)
    local t0=$SECONDS
    demo_flow "$root/scripts/demo-story/story.drive" "$demo_wallet" 2400 || {
        rec_stop
        die "the story failed after $((SECONDS - t0))s; logs in $demo_work"
    }
    rec_stop
    say "story done in $((SECONDS - t0))s"
    join_segments
    # The dark theme, on the wallet the story left.
    demo_on_shot() {
        local name
        case "$1" in
            dark-spending) name=25-dark-spending ;;
            dark-saving) name=26-dark-saving ;;
            dark-settings) name=27-dark-settings ;;
            *) return 0 ;;
        esac
        demo_grim "$shots/$name.png"
        say "screenshot $name.png"
    }
    ADW_DEBUG_COLOR_SCHEME=prefer-dark demo_flow "$root/scripts/demo-story/dark.drive" "$demo_wallet" 300 ||
        say "the dark-theme screenshots failed; the video and light screenshots stand"
}

case "${1:-}" in
    --inner)
        inner
        ;;
    -h | --help)
        sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
        ;;
    "")
        demo_enter_shell "$0" "$@"
        demo_check_shell
        if ! command -v wf-recorder >/dev/null 2>&1 || ! command -v ffmpeg >/dev/null 2>&1; then
            echo "fetching wf-recorder and ffmpeg from nixpkgs..." >&2
            tools=$(nix build --no-link --print-out-paths nixpkgs#wf-recorder nixpkgs#ffmpeg.bin) ||
                die "could not fetch wf-recorder and ffmpeg (nix build nixpkgs#wf-recorder nixpkgs#ffmpeg)"
            for p in $tools; do PATH="$p/bin:$PATH"; done
            export PATH
        fi
        demo_check_shell wf-recorder ffmpeg ffprobe
        demo_stop_previous
        demo_fresh_state
        mkdir -p "$shots"
        rm -f "$shots"/[0-9][0-9]-*.png "$video"
        demo_build
        echo "starting the regtest devimint environment (log: $demo_state/devimint.log)..." >&2
        trap 'true' INT
        status=0
        demo_devimint "$demo_state/devimint.log" "$root/scripts/record-demo.sh" --inner || status=$?
        test_dir=$(sed -n 's/^declare -gx FM_TEST_DIR="\(.*\)"$/\1/p' "$demo_env_file" 2>/dev/null || true)
        demo_kill_test_dir "$test_dir"
        if [[ "$status" != 0 ]]; then
            echo "recording failed (exit $status); devimint log: $demo_state/devimint.log, app logs: $demo_work" >&2
            exit "$status"
        fi
        ls -l "$video" "$shots" >&2
        ;;
    *)
        echo "usage: scripts/record-demo.sh" >&2
        exit 2
        ;;
esac
