#!/usr/bin/env bash
# Render the app in a headless sway session and save a screenshot.
# Usage: scripts/screenshot.sh <out.png> [--width W] [--height H] -- <fedimix args>
set -euo pipefail

# GTK reports the binary name as app_id here, not the application id.
APP_ID="fedimix"
width=1280
height=720

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <out.png> [--width W] [--height H] -- <fedimix args>" >&2
    exit 2
fi
out="$1"
shift
while [[ $# -gt 0 ]]; do
    case "$1" in
        --width) width="$2"; shift 2 ;;
        --height) height="$2"; shift 2 ;;
        --) shift; break ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
app_args=("$@")

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$(dirname "$out")"
out="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"

# Keep the runtime dir path short: long paths break the Wayland socket.
rt="$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}")"
sway_pid=""
app_pid=""
cleanup() {
    [[ -n "$app_pid" ]] && kill "$app_pid" 2>/dev/null || true
    [[ -n "$sway_pid" ]] && kill "$sway_pid" 2>/dev/null || true
    rm -rf "$rt"
}
trap cleanup EXIT

cat > "$rt/sway.conf" <<CONF
output * resolution ${width}x${height}
output * bg #333333 solid_color
CONF

# Build first so the 15 s window wait does not include compilation.
(cd "$root" && cargo build -q -p fedimix-gtk)
bin="$root/target/debug/fedimix"

export XDG_RUNTIME_DIR="$rt"
WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    sway -c "$rt/sway.conf" --unsupported-gpu >"$rt/sway.log" 2>&1 &
sway_pid=$!

# Discover the Wayland socket and sway IPC socket.
for _ in $(seq 100); do
    wl=$(cd "$rt" && ls wayland-* 2>/dev/null | grep -v '\.lock$' | head -n1 || true)
    sock=$(ls "$rt"/sway-ipc.*.sock 2>/dev/null | head -n1 || true)
    [[ -n "$wl" && -n "$sock" ]] && break
    sleep 0.1
done
if [[ -z "${wl:-}" || -z "${sock:-}" ]]; then
    echo "sway did not start:" >&2
    exit 1
fi
export WAYLAND_DISPLAY="$wl" SWAYSOCK="$sock"

GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none \
    "$bin" "${app_args[@]}" >"$rt/app.log" 2>&1 &
app_pid=$!

mapped=0
for _ in $(seq 150); do
    if swaymsg -t get_tree 2>/dev/null | jq -e --arg id "$APP_ID" \
        '[.. | objects | select(.app_id? == $id)] | length > 0' >/dev/null; then
        mapped=1
        break
    fi
    kill -0 "$app_pid" 2>/dev/null || break
    sleep 0.1
done
if [[ $mapped -ne 1 ]]; then
    echo "window $APP_ID never mapped:" >&2
    cat "$rt/app.log" >&2
    exit 1
fi
sleep 1
grim "$out"
echo "wrote $out"
