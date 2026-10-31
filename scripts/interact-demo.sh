#!/usr/bin/env bash
# Run scripted UI flows (scripts/demo-flows/*.drive) through the in-app demo
# driver under headless sway, taking a grim screenshot at every `shot` step.
# Usage: scripts/interact-demo.sh [flow-name ...]   (default: all flows)
# Flow header comments: `# scenario: <demo scenario>`, `# page: <demo page>`,
# `# size: WxH`, `# step-ms: N` (time in each operation state,
# for flows whose screenshot would outlast the demo's 300 ms).
# Output: target/screenshots/interact/<flow>-<shot>.png
set -euo pipefail

APP_ID="fedimix"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
outdir="$root/target/screenshots/interact"
mkdir -p "$outdir"

(cd "$root" && cargo build -q -p fedimix-gtk)
bin="$root/target/debug/fedimix"

if [[ $# -gt 0 ]]; then
    flows=()
    for n in "$@"; do flows+=("$root/scripts/demo-flows/$n.drive"); done
else
    flows=("$root"/scripts/demo-flows/*.drive)
fi

rt=""
sway_pid=""
app_pid=""
cleanup() {
    [[ -n "$app_pid" ]] && kill "$app_pid" 2>/dev/null || true
    [[ -n "$sway_pid" ]] && kill "$sway_pid" 2>/dev/null || true
    exec 3>&- 2>/dev/null || true
    if [[ -n "$rt" ]]; then rm -rf "$rt"; fi
}
trap cleanup EXIT

run_flow() {
    local file="$1" name scenario page size width height step
    name="$(basename "$file" .drive)"
    scenario="$(sed -n 's/^# scenario: *//p' "$file" | head -n1)"
    page="$(sed -n 's/^# page: *//p' "$file" | head -n1)"
    size="$(sed -n 's/^# size: *//p' "$file" | head -n1)"
    step="$(sed -n 's/^# step-ms: *//p' "$file" | head -n1)"
    scenario="${scenario:-funded}"
    size="${size:-480x640}"
    width="${size%x*}"
    height="${size#*x}"

    rt="$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}")"
    printf 'output * resolution %sx%s\noutput * bg #333333 solid_color\n' \
        "$width" "$height" > "$rt/sway.conf"
    unset WAYLAND_DISPLAY SWAYSOCK
    export XDG_RUNTIME_DIR="$rt"
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
        sway -c "$rt/sway.conf" --unsupported-gpu >"$rt/sway.log" 2>&1 &
    sway_pid=$!
    local wl="" sock=""
    for _ in $(seq 100); do
        wl=$(cd "$rt" && ls wayland-* 2>/dev/null | grep -v '\.lock$' | head -n1 || true)
        sock=$(ls "$rt"/sway-ipc.*.sock 2>/dev/null | head -n1 || true)
        [[ -n "$wl" && -n "$sock" ]] && break
        sleep 0.1
    done
    [[ -n "$wl" && -n "$sock" ]] || { echo "sway did not start" >&2; return 1; }
    export WAYLAND_DISPLAY="$wl" SWAYSOCK="$sock"

    : > "$rt/out.log"
    mkfifo "$rt/stdin"
    exec 3<>"$rt/stdin"
    GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none \
        "$bin" --demo --demo-scenario "$scenario" ${page:+--demo-page "$page"} ${step:+--demo-step-ms "$step"} \
        --demo-drive "$file" \
        <&3 >"$rt/out.log" 2>"$rt/err.log" &
    app_pid=$!

    local seen=0 status="" deadline=$((SECONDS + 90)) line
    while [[ -z "$status" ]]; do
        local total
        total=$(wc -l < "$rt/out.log")
        while [[ $seen -lt $total ]]; do
            seen=$((seen + 1))
            line="$(sed -n "${seen}p" "$rt/out.log")"
            case "$line" in
                "SHOT "*)
                    sleep 0.3
                    grim "$outdir/$name-${line#SHOT }.png"
                    echo "wrote $outdir/$name-${line#SHOT }.png"
                    echo go >&3
                    ;;
                "DRIVE OK") status=ok ;;
                "DRIVE FAILED"*) status="failed: $line" ;;
            esac
        done
        if [[ -z "$status" ]]; then
            kill -0 "$app_pid" 2>/dev/null || status="app exited early"
            [[ $SECONDS -lt $deadline ]] || status="timed out"
            sleep 0.1
        fi
    done
    wait "$app_pid" 2>/dev/null || true
    app_pid=""
    exec 3>&-
    kill "$sway_pid" 2>/dev/null || true
    sway_pid=""

    local bad=""
    if grep -Eq 'CRITICAL|panicked' "$rt/err.log"; then
        bad="CRITICAL/panic in stderr"
        grep -E 'CRITICAL|panicked' "$rt/err.log" >&2
    fi
    if [[ "$status" != ok ]] || [[ -n "$bad" ]]; then
        echo "FLOW $name: ${status#ok}${bad:+ $bad}" >&2
        cat "$rt/out.log" "$rt/err.log" >&2
        rm -rf "$rt"; rt=""
        return 1
    fi
    echo "FLOW $name: ok"
    rm -rf "$rt"; rt=""
}

failed=0
for f in "${flows[@]}"; do
    run_flow "$f" || failed=1
done
exit $failed
