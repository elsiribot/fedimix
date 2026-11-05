#!/usr/bin/env bash
# Live demo of Fedimix on a local REGTEST devimint environment (not signet:
# no public signet federation is usable). Starts a federation of one, bitcoind,
# LND and LDK Lightning gateways, Esplora and a background miner; pre-funds a
# demo data folder headlessly (joined, 0.0019 BTC of ecash, Saving on "This
# computer" holding 0.01 BTC); then opens the real `fedimix` window on your
# desktop. Ctrl-C tears everything down. Running it again stops an earlier run
# first and starts clean.
#
# usage: scripts/demo-live.sh             start (or restart) the live demo
#        scripts/demo-live.sh pay BOLT11   pay a Lightning invoice into Fedimix
#        scripts/demo-live.sh send ADDRESS [BTC]   send on chain (default 0.01) and mine 1 block
#        scripts/demo-live.sh mine [N]     mine N blocks (default 6)
#        scripts/demo-live.sh open         open the window again after closing it
#        scripts/demo-live.sh info         print the invite code, chain URL and helpers
# The helpers work from any terminal while the demo runs; no nix shell needed.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mode=live
# shellcheck source=demo-lib.sh
source "$root/scripts/demo-lib.sh"

# Block interval while the window is up, and while pre-funding.
block_secs="${FEDIMIX_DEMO_BLOCK_SECS:-5}"
prefund_block_secs=2

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }

# The helpers read what the running demo wrote down.
load_env() {
    [[ -f "$demo_env_file" ]] || die "no live demo is running. Start it with: just demo-live"
    # shellcheck disable=SC1090
    source "$demo_env_file"
    local pid=""
    [[ -f "$demo_pid_file" ]] && pid=$(cat "$demo_pid_file")
    [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null ||
        die "the live demo is not running any more. Start it with: just demo-live"
}

print_info() {
    local me="$root/scripts/demo-live.sh"
    say ""
    say "================ Fedimix live demo: REGTEST (local devimint) ================"
    say "Invite code:  $FM_INVITE_CODE"
    say "Chain URL:    $FEDIMIX_CHAIN_URL   (Esplora, regtest; set in Settings > Server URL)"
    say "Data folder:  $demo_wallet"
    say "Miner:        1 block every ${block_secs}s"
    say ""
    say "Helpers, from any terminal:"
    say "  Pay a Lightning invoice into Fedimix:  $me pay <lnbcrt... invoice>"
    say "  Send on chain to a Fedimix address:    $me send <bcrt1... address> [BTC, default 0.01]"
    say "  Mine blocks:                           $me mine [N, default 6]"
    say "  Reopen the window after closing it:    $me open"
    say "  (just demo-live pay|send|mine|open ... works too)"
    say "Ctrl-C here stops the window, the miner and the whole devimint environment."
    say "============================================================================="
}

# The window on the desktop this was started from, not the headless sway.
open_window() {
    local vars=(XDG_RUNTIME_DIR="${FEDIMIX_DEMO_DESKTOP_XDG:-${XDG_RUNTIME_DIR:-}}")
    local unset=(-u SWAYSOCK)
    if [[ -n "${FEDIMIX_DEMO_DESKTOP_WAYLAND:-${WAYLAND_DISPLAY:-}}" ]]; then
        vars+=(WAYLAND_DISPLAY="${FEDIMIX_DEMO_DESKTOP_WAYLAND:-$WAYLAND_DISPLAY}")
    else
        unset+=(-u WAYLAND_DISPLAY)
    fi
    if [[ -n "${FEDIMIX_DEMO_DESKTOP_X11:-${DISPLAY:-}}" ]]; then
        vars+=(DISPLAY="${FEDIMIX_DEMO_DESKTOP_X11:-$DISPLAY}")
    fi
    env "${unset[@]}" "${vars[@]}" LD_LIBRARY_PATH="$FEDIMIX_UI_LD_LIBRARY_PATH" \
        "$demo_app" --data-dir "$demo_wallet" --network regtest "$@"
}

# ---- Inside `devimint dev-fed --exec` ----

app_pid=""
cleanup() {
    [[ -n "$app_pid" ]] && kill "$app_pid" 2>/dev/null || true
    [[ -n "${demo_flow_pid:-}" ]] && kill "$demo_flow_pid" 2>/dev/null || true
    [[ -n "$demo_miner_pid" ]] && kill "$demo_miner_pid" 2>/dev/null || true
    [[ -n "$demo_sway_pid" ]] && kill "$demo_sway_pid" 2>/dev/null || true
    rm -f "$demo_pid_file"
}

demo_on_shot() {
    case "$1" in
        invoice) demo_pay_invoice "$(cat "$demo_work/invoice.txt")" ;;
        saving-address) demo_send_onchain "$(cat "$demo_work/saving-address.txt")" 0.01 ;;
    esac
}

prefund() {
    say "pre-funding the demo wallet headlessly (join, Lightning receive, Saving setup and receive)..."
    (
        trap '[[ -n "$demo_sway_pid" ]] && kill "$demo_sway_pid" 2>/dev/null' EXIT
        demo_start_sway "$demo_work/sway" 1280x800
        demo_flow "$root/scripts/demo-story/prefund.drive" "$demo_wallet" 900
    )
}

inner() {
    trap cleanup EXIT
    trap 'exit 130' INT TERM
    echo $$ > "$demo_pid_file"
    export FEDIMIX_CHAIN_URL="http://127.0.0.1:$FM_PORT_ESPLORA"
    demo_write_env
    echo "$started_marker"
    say "regtest federation up. Invite code: $FM_INVITE_CODE"
    demo_start_miner "$prefund_block_secs"
    local t0=$SECONDS
    prefund || die "pre-funding failed; logs in $demo_work"
    say "pre-funded in $((SECONDS - t0))s"
    kill "$demo_miner_pid" 2>/dev/null || true
    demo_start_miner "$block_secs"
    print_info
    while true; do
        open_window &
        app_pid=$!
        wait "$app_pid" || true
        app_pid=""
        say "The window closed. The environment stays up: '$root/scripts/demo-live.sh open' reopens it, Ctrl-C stops it."
        sleep infinity &
        wait $! || true
    done
}

case "${1:-start}" in
    pay)
        load_env
        demo_pay_invoice "${2:?usage: demo-live.sh pay <bolt11 invoice>}"
        ;;
    send)
        load_env
        demo_send_onchain "${2:?usage: demo-live.sh send <address> [BTC]}" "${3:-0.01}"
        ;;
    mine)
        load_env
        mine "${2:-6}"
        echo "mined ${2:-6} blocks"
        ;;
    open)
        load_env
        open_window
        ;;
    info)
        load_env
        print_info
        ;;
    --inner)
        inner
        ;;
    -h | --help)
        usage
        ;;
    start)
        # The checks that need no nix come first, so a missing desktop fails fast.
        if [[ -z "${WAYLAND_DISPLAY:-}" && -z "${DISPLAY:-}" ]]; then
            die "no desktop session (neither WAYLAND_DISPLAY nor DISPLAY is set). Run this from a terminal on your desktop, or record headlessly with: just record-demo"
        fi
        if [[ -z "${FEDIMIX_DEMO_IN_SHELL:-}" ]]; then
            export FEDIMIX_DEMO_DESKTOP_XDG="${XDG_RUNTIME_DIR:-}"
            export FEDIMIX_DEMO_DESKTOP_WAYLAND="${WAYLAND_DISPLAY:-}"
            export FEDIMIX_DEMO_DESKTOP_X11="${DISPLAY:-}"
        fi
        demo_enter_shell "$0" "$@"
        demo_check_shell
        demo_stop_previous
        demo_fresh_state
        demo_build
        echo "starting the regtest devimint environment (about a minute; log: $demo_state/devimint.log)..." >&2
        # A handler, not an ignore, so devimint and the app still get Ctrl-C.
        trap 'true' INT
        status=0
        demo_devimint "$demo_state/devimint.log" "$root/scripts/demo-live.sh" --inner || status=$?
        test_dir=$(sed -n 's/^declare -gx FM_TEST_DIR="\(.*\)"$/\1/p' "$demo_env_file" 2>/dev/null || true)
        demo_kill_test_dir "$test_dir"
        rm -f "$demo_pid_file"
        echo "live demo stopped." >&2
        if [[ "$status" != 0 && "$status" != 130 ]] && ! grep -qF "$started_marker" "$demo_state/devimint.log"; then
            echo "devimint did not start; the end of $demo_state/devimint.log:" >&2
            tail -n 30 "$demo_state/devimint.log" >&2
            exit "$status"
        fi
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
