#!/usr/bin/env bash
# Records the Phase 4 exits story on the DEMO backend (simulated: the real
# backend does not do exits yet), headlessly: its own sway at 1280x800, the
# real `fedimix --demo --demo-scenario exits` driven by
# scripts/demo-story/exits.drive, wf-recorder for the video. Adds a title card,
# a closing card and a "SIMULATED" corner label with ffmpeg, so the video says
# what it is. Needs no federation: no devimint, no network.
#
# usage: scripts/record-exits-demo.sh   (or: just record-exits-demo)
# Output (FEDIMIX_DEMO_OUT overrides the folder):
#   /home/user/fedimix-demo/phase4/exits-demo.mp4
#   /home/user/fedimix-demo/phase4/screenshots/NN-name.png
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mode=exits
# shellcheck source=demo-lib.sh
source "$root/scripts/demo-lib.sh"

out_dir="${FEDIMIX_DEMO_OUT:-$HOME/fedimix-demo/phase4}"
shots="$out_dir/screenshots"
video="$out_dir/exits-demo.mp4"
size=1280x800
step_ms=12000

rec_pid=""
app_pid=""
sway_dir="$demo_work/sway"

cleanup() {
    [[ -n "$rec_pid" ]] && kill -INT "$rec_pid" 2>/dev/null || true
    [[ -n "$app_pid" ]] && kill "$app_pid" 2>/dev/null || true
    [[ -n "$demo_sway_pid" ]] && kill "$demo_sway_pid" 2>/dev/null || true
}

# Card text (no colons or commas: they break the filter text): a dark 1280x800 slide, lines centred. usage: card <secs> <out> <font> <line>...
card() {
    local secs="$1" file="$2" font="$3" filter="" y=${CARD_Y:-260} line
    shift 3
    for line in "$@"; do
        filter+="drawtext=fontfile=$font:text='$line':fontcolor=white:fontsize=${CARD_SIZE:-44}:x=(w-text_w)/2:y=$y,"
        y=$((y + 80))
    done
    ffmpeg -loglevel error -y -f lavfi -i "color=c=0x242424:s=$size:r=30:d=$secs" \
        -vf "${filter%,}" -c:v libx264 -pix_fmt yuv420p "$file"
}

record() {
    local raw="$demo_work/raw.mp4" out name drive="$root/scripts/demo-story/exits.drive"
    demo_start_sway "$sway_dir" "$size"
    out=$(demo_output_name)
    local stdin="$demo_work/stdin" log="$demo_work/app.out"
    rm -f "$stdin"; mkfifo "$stdin"
    exec {fd}<>"$stdin"
    : > "$log"
    (cd "$demo_work" && GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none \
        exec "$demo_app" --demo --demo-scenario exits --demo-step-ms "$step_ms" --demo-drive "$drive") \
        <&"$fd" >"$log" 2>"$demo_work/app.err" &
    app_pid=$!
    local seen=0 status="" line total deadline=$((SECONDS + 400))
    while [[ -z "$status" ]]; do
        total=$(wc -l < "$log")
        while [[ $seen -lt $total ]]; do
            seen=$((seen + 1))
            line="$(sed -n "${seen}p" "$log")"
            case "$line" in
                "SHOT _rec")
                    # -D: every frame, so a still screen keeps its time.
                    wf-recorder -D -o "$out" -f "$raw" -c libx264 -x yuv420p -r 30 \
                        </dev/null >"$demo_work/wf-recorder.log" 2>&1 &
                    rec_pid=$!
                    sleep 1
                    echo go >&"$fd"
                    ;;
                "SHOT "*)
                    name="${line#SHOT }"
                    demo_grim "$shots/$name.png"
                    say "screenshot $name.png"
                    echo go >&"$fd"
                    ;;
                "DRIVE OK" | "DRIVE FAILED"*) status="$line" ;;
            esac
        done
        if [[ -z "$status" ]]; then
            kill -0 "$app_pid" 2>/dev/null || status="exited"
            [[ $SECONDS -lt $deadline ]] || status="timed out"
            sleep 0.1
        fi
    done
    sleep 1
    kill -INT "$rec_pid" 2>/dev/null || true
    wait "$rec_pid" 2>/dev/null || true
    rec_pid=""
    [[ "$status" == "DRIVE OK" ]] || { tail -n 20 "$log" "$demo_work/app.err" >&2; die "the drive ended: $status"; }
    wait "$app_pid" || die "the app exited with an error (see $demo_work/app.err)"
    app_pid=""
    ! grep -Eq 'CRITICAL|panicked' "$demo_work/app.err" || die "CRITICAL or panic in $demo_work/app.err"
}

compose() {
    local font="$1" raw="$demo_work/raw.mp4"
    CARD_SIZE=40 card 7 "$demo_work/title.mp4" "$font" \
        "Fedimix - automatic private exits" \
        "Spending to Saving - Phase 4" \
        "THIS PART RUNS ON THE DEMO BACKEND" \
        "(SIMULATED)" \
        "The real backend does not do exits yet"
    CARD_SIZE=40 card 5 "$demo_work/end.mp4" "$font" \
        "Simulated - demo backend (scenario exits)" \
        "Real exits come with the real backend"
    # wf-recorder writes limited-range frames but tags them full range (yuvj420p):
    # re-tag the stream, or the whole video comes out about 8% too dark.
    ffmpeg -loglevel error -y -i "$raw" -c:v copy -bsf:v h264_metadata=video_full_range_flag=0 "$demo_work/raw-tv.mp4"
    # A corner label on the app footage, so it is never mistaken for the real thing.
    ffmpeg -loglevel error -y -i "$demo_work/raw-tv.mp4" -vf \
        "drawtext=fontfile=$font:text='SIMULATED - demo backend':fontcolor=white:fontsize=20:box=1:boxcolor=0xb00020@0.85:boxborderw=8:x=w-text_w-16:y=h-text_h-16" \
        -c:v libx264 -pix_fmt yuv420p -r 30 "$demo_work/body.mp4"
    printf "file '%s'\nfile '%s'\nfile '%s'\n" "$demo_work/title.mp4" "$demo_work/body.mp4" "$demo_work/end.mp4" > "$demo_work/list.txt"
    ffmpeg -loglevel error -y -f concat -safe 0 -i "$demo_work/list.txt" \
        -c:v libx264 -pix_fmt yuv420p -r 30 -movflags +faststart "$video"
    local secs
    secs=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$video")
    say "video: $video (${secs%.*} s)"
}

case "${1:-}" in
    --inner)
        trap cleanup EXIT
        trap 'exit 130' INT TERM
        demo_build
        mkdir -p "$shots"
        rm -f "$shots"/[0-9][0-9]-*.png "$video"
        record
        compose "$FONT"
        ;;
    -h | --help)
        sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
        ;;
    "")
        demo_check_host
        demo_fresh_state
        extra=$(nix build --no-link --print-out-paths nixpkgs#wf-recorder nixpkgs#ffmpeg.bin nixpkgs#dejavu_fonts) ||
            die "could not fetch wf-recorder, ffmpeg and a font from nixpkgs"
        for p in $extra; do [[ -d "$p/bin" ]] && PATH="$p/bin:$PATH"; done
        FONT=$(find $extra -name DejaVuSans-Bold.ttf | head -n1)
        export PATH FONT
        [[ -n "$FONT" ]] || die "no font found"
        exec nix develop "$root" -c "$0" --inner
        ;;
    *)
        echo "usage: scripts/record-exits-demo.sh" >&2
        exit 2
        ;;
esac
