#!/usr/bin/env bash
# Drives the real binary against a devimint federation under headless sway: Join
# from the invite, receive over Lightning from LND, restart, a second launch on
# the same folder, a Lightning payment to a second wallet, and the folder refusing
# another network. Needs the
# `integration` dev shell; see `just test-real-ui`.
# Output: target/screenshots/real/<flow>-<shot>.png
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"

# How many times devimint is started again after it failed to start.
max_retries=2
# Fresh invoices a payment to the second wallet gets: a gateway can fail.
pay_tries=3
started="=== real UI steps start ==="
passed="=== real UI steps passed ==="

# ---- Inside `devimint dev-fed --exec` ----

work=""
sway_pid=""
app_pids=()

cleanup() {
    for pid in ${app_pids[@]+"${app_pids[@]}"}; do
        kill "$pid" 2>/dev/null || true
    done
    [[ -n "$sway_pid" ]] && kill "$sway_pid" 2>/dev/null || true
    if [[ -n "$work" ]]; then rm -rf "$work"; fi
}

start_sway() {
    # Wide enough for two windows side by side at their minimum width.
    printf 'output * resolution 800x640\noutput * bg #333333 solid_color\n' > "$work/sway.conf"
    unset WAYLAND_DISPLAY SWAYSOCK
    export XDG_RUNTIME_DIR="$work"
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
        sway -c "$work/sway.conf" --unsupported-gpu >"$work/sway.log" 2>&1 &
    sway_pid=$!
    local wl="" sock=""
    for _ in $(seq 100); do
        wl=$(cd "$work" && ls wayland-* 2>/dev/null | grep -v '\.lock$' | head -n1 || true)
        sock=$(ls "$work"/sway-ipc.*.sock 2>/dev/null | head -n1 || true)
        [[ -n "$wl" && -n "$sock" ]] && break
        sleep 0.1
    done
    [[ -n "$wl" && -n "$sock" ]] || { echo "sway did not start" >&2; return 1; }
    export WAYLAND_DISPLAY="$wl" SWAYSOCK="$sock"
}

# usage: flow <name> <wanted exit code> <wanted final line>
# Runs scripts/real-flows/<name>.drive on the wallet's folder, answering its
# `shot` steps, and calls `on_shot_<name> <shot>` for each if that is defined.
# `exited` is the final line of an app that never printed a DRIVE line. Returns 1
# unless both match. The network is $flow_network, regtest by default.
flow() {
    local name="$1" wanted_exit="$2" wanted_line="$3"
    local out="$work/$name.out" err="$work/$name.err" stdin="$work/$name.stdin"
    local fd pid seen=0 total line status="" deadline=$((SECONDS + 400)) code=0
    : > "$out"
    rm -f "$stdin"
    mkfifo "$stdin"
    exec {fd}<>"$stdin"
    (cd "$work" && LD_LIBRARY_PATH="${FEDIMIX_UI_LD_LIBRARY_PATH:-}" \
        GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none \
        exec "$root/target/debug/fedimix" --data-dir "$work/wallet" --network "${flow_network:-regtest}" \
        --demo-drive "$root/scripts/real-flows/$name.drive") \
        <&"$fd" >"$out" 2>"$err" &
    pid=$!
    app_pids+=("$pid")

    while [[ -z "$status" ]]; do
        total=$(wc -l < "$out")
        while [[ $seen -lt $total ]]; do
            seen=$((seen + 1))
            line="$(sed -n "${seen}p" "$out")"
            case "$line" in
                "SHOT "*)
                    sleep 0.3
                    grim "$root/target/screenshots/real/$name-${line#SHOT }.png"
                    echo "wrote target/screenshots/real/$name-${line#SHOT }.png"
                    if declare -F "on_shot_$name" >/dev/null; then
                        "on_shot_$name" "${line#SHOT }"
                    fi
                    echo go >&"$fd"
                    ;;
                "DRIVE OK" | "DRIVE FAILED"*) status="$line" ;;
            esac
        done
        if [[ -z "$status" ]]; then
            kill -0 "$pid" 2>/dev/null || status="exited"
            [[ $SECONDS -lt $deadline ]] || status="timed out"
            sleep 0.1
        fi
    done
    [[ "$status" != "timed out" ]] || kill "$pid" 2>/dev/null || true
    wait "$pid" || code=$?
    exec {fd}>&-

    local bad=""
    [[ "$code" == "$wanted_exit" ]] || bad="exit code $code, wanted $wanted_exit"
    [[ "$status" == "$wanted_line" ]] || bad="${bad:+$bad; }ended with '$status', wanted '$wanted_line'"
    if grep -Eq 'CRITICAL|panicked' "$err"; then
        bad="${bad:+$bad; }CRITICAL or panic in stderr"
    fi
    if [[ -n "$bad" ]]; then
        echo "FLOW $name: $bad" >&2
        tail -n 30 "$out" "$err" >&2
        return 1
    fi
    echo "FLOW $name: ok (exit $code)"
}

# LND cannot pay an invoice from its own gateway and the wallet picks one at
# random, so the drive makes several and the first one that works ends this.
paid=""
on_shot_join-and-receive() {
    local shot="$1"
    [[ "$shot" == invoice-* && -z "$paid" ]] || return 0
    if timeout 30 $FM_LNCLI payinvoice --force "$(cat "$work/$shot.txt")" >/dev/null; then
        paid="$shot"
        echo "LND paid $shot"
    else
        echo "LND could not pay $shot"
    fi
}

# While a first app holds the folder: a second one says so and exits 1.
on_shot_reopen() {
    [[ "$1" == reopened ]] || return 0
    flow second-launch 1 "exited"
    grep -q 'Fedimix is already running with this data folder.' "$work/second-launch.err" || {
        echo "the second launch did not report the lock" >&2
        return 1
    }
}

on_second_wallet_paid() {
    # `await-receive` ends once the second wallet holds the money or never will.
    local outcome
    outcome=$($FM_MINT_CLIENT module lnv2 await-receive "$1")
    echo "second wallet: $outcome"
    grep -q 'Claimed' <<<"$outcome"
}

pay_second_wallet() {
    local try reply operation
    for try in $(seq 1 "$pay_tries"); do
        reply=$($FM_MINT_CLIENT module lnv2 receive 50000000)
        export FEDIMIX_SECOND_INVOICE
        FEDIMIX_SECOND_INVOICE=$(jq -r '.[0]' <<<"$reply")
        operation=$(jq -r '.[1]' <<<"$reply")
        if flow pay-second-wallet 0 "DRIVE OK"; then
            on_second_wallet_paid "$operation"
            return
        fi
        echo "payment try $try did not settle" >&2
    done
    echo "no payment settled in $pay_tries tries" >&2
    return 1
}

inner() {
    trap cleanup EXIT
    work="$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}")"
    rm -rf "$root/target/screenshots/real"
    mkdir -p "$root/target/screenshots/real"
    echo "$started"
    start_sway
    flow join-and-receive 0 "DRIVE OK"
    [[ -n "$paid" ]] || { echo "LND paid no invoice" >&2; return 1; }
    flow reopen 0 "DRIVE OK"
    pay_second_wallet
    flow_network=signet flow wrong-network 1 "exited"
    echo "$passed"
}

if [[ "${1:-}" == --inner ]]; then
    inner
    exit
fi

# ---- Outside: same shape as scripts/test-integration.sh ----

cargo build -q -p fedimix-gtk

out="$(mktemp)"
trap 'rm -f "$out"' EXIT

# devimint fails about 1 run in 5 at `gateway-cli ... open-channel`, before any
# step runs. Only that is worth another try.
failed_at_startup() {
    ! grep -qF "$started" "$out" && grep -q 'open-channel' "$out"
}

retries=0
while true; do
    echo "=== devimint: default modules (mintv2, walletv2) ==="
    status=0
    env -u FM_ENABLE_MODULE_MINT -u FM_ENABLE_MODULE_WALLET \
        -u FM_ENABLE_MODULE_MINTV2 -u FM_ENABLE_MODULE_WALLETV2 \
        FM_FED_SIZE=1 \
        devimint dev-fed --exec \
        scripts/fund-gateways.sh \
        scripts/test-real-ui.sh --inner \
        2>&1 | tee "$out" || status=$?
    if [ "$status" = 0 ]; then
        break
    fi
    if [ "$retries" -ge "$max_retries" ] || ! failed_at_startup; then
        exit "$status"
    fi
    retries=$((retries + 1))
    echo "devimint failed at open-channel before any step; retry $retries of $max_retries" >&2
done
# A run that skipped its steps exits 0 too, so require the last line of them.
if ! grep -qF "$passed" "$out"; then
    echo "the real UI steps did not finish" >&2
    exit 1
fi
