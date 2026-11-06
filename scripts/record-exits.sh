#!/usr/bin/env bash
# Records exits on the REAL backend, headlessly, on a local REGTEST devimint
# environment: its own sway at 1280x800, the real `fedimix` (testing build,
# `--exit-timing fast`, Saving on "This computer") driven by
# scripts/real-flows/exits-demo.drive, wf-recorder for the video. The story:
# join, a deposit above the target, the queue and its meters, an early exit
# with its fee, a second deposit that leaves on its own, the Coins tags. Waits
# on blocks and on the scheduler are cut out. No text is added to the video.
# The local devimint has no other depositors, so every anonymity estimate
# reads 0 here. For the simulated exits on the demo backend, see
# scripts/record-exits-demo.sh.
#
# usage: scripts/record-exits.sh   (or: just record-exits)
# Output (FEDIMIX_DEMO_OUT overrides the folder):
#   /home/user/fedimix-demo/phase4/real-exits/exits.mp4
#   /home/user/fedimix-demo/phase4/real-exits/screenshots/NN-name.png
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mode=record-exits
# shellcheck source=demo-lib.sh
source "$root/scripts/demo-lib.sh"

out_dir="${FEDIMIX_DEMO_OUT:-$HOME/fedimix-demo/phase4/real-exits}"
shots="$out_dir/screenshots"
video="$out_dir/exits.mp4"
size=1280x800

# ---- Inside `devimint dev-fed --exec` ----

cleanup() {
    [[ -n "$rec_pid" ]] && kill -INT "$rec_pid" 2>/dev/null || true
    [[ -n "${demo_flow_pid:-}" ]] && kill "$demo_flow_pid" 2>/dev/null || true
    [[ -n "$demo_miner_pid" ]] && kill "$demo_miner_pid" 2>/dev/null || true
    [[ -n "$demo_sway_pid" ]] && kill "$demo_sway_pid" 2>/dev/null || true
    rm -f "$demo_pid_file"
}

# Pays the deposit address the drive saved, in the background: the app waits
# at most 10 s for a shot's answer.
pay_deposit() {
    local file="$1" amount="$2"
    (demo_send_onchain "$(cat "$demo_work/$file")" "$amount" >/dev/null) &
}

demo_on_shot() {
    local shot="$1"
    case "$shot" in
        _rec) demo_rec_start; return ;;
        _cut) demo_rec_stop; return ;;
    esac
    demo_grim "$shots/$shot.png"
    say "screenshot $shot.png"
    case "$shot" in
        # 0.01 above the 0.002 target, with room for the deposit fee and the exit's.
        03-deposit-1) pay_deposit exits-deposit-1.txt 0.0125 ;;
        09-deposit-2) pay_deposit exits-deposit-2.txt 0.0205 ;;
    esac
}

inner() {
    trap cleanup EXIT
    trap 'exit 130' INT TERM
    echo $$ > "$demo_pid_file"
    export FEDIMIX_CHAIN_URL="http://127.0.0.1:$FM_PORT_ESPLORA"
    demo_write_env
    echo "$started_marker"
    say "regtest federation up; recording exits (several minutes, most of it cut)..."
    demo_start_sway "$demo_work/sway" "$size"
    output=$(demo_output_name)
    say "recording sway output $output at $size"
    demo_start_miner 2
    export FEDIMIX_SAVING_DEST
    FEDIMIX_SAVING_DEST=$(btc getnewaddress)
    local t0=$SECONDS
    demo_flow_args="--exit-timing fast" \
        demo_flow "$root/scripts/real-flows/exits-demo.drive" "$demo_wallet" 1800 || {
        demo_rec_stop
        die "the exits story failed after $((SECONDS - t0))s; logs in $demo_work"
    }
    demo_rec_stop
    say "story done in $((SECONDS - t0))s"
    demo_join_segments
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
        # --exit-timing needs the testing feature.
        demo_features=fedimix-gtk/testing demo_build
        echo "starting the regtest devimint environment (log: $demo_state/devimint.log)..." >&2
        trap 'true' INT
        status=0
        demo_devimint "$demo_state/devimint.log" "$root/scripts/record-exits.sh" --inner || status=$?
        test_dir=$(sed -n 's/^declare -gx FM_TEST_DIR="\(.*\)"$/\1/p' "$demo_env_file" 2>/dev/null || true)
        demo_kill_test_dir "$test_dir"
        if [[ "$status" != 0 ]]; then
            echo "recording failed (exit $status); devimint log: $demo_state/devimint.log, app logs: $demo_work" >&2
            exit "$status"
        fi
        ls -l "$video" "$shots" >&2
        ;;
    *)
        echo "usage: scripts/record-exits.sh" >&2
        exit 2
        ;;
esac
