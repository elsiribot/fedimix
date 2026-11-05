# Shared by scripts/demo-live.sh and scripts/record-demo.sh; sourced, not run.
#
# Everything here runs on a local REGTEST devimint environment: a federation of
# one guardian, bitcoind, LND and LDK Lightning gateways and Esplora, all on
# this machine with dynamic ports. Nothing here touches signet or mainnet.
#
# The caller sets `root` (the repository) and `mode` (live or record) first.

# Where each mode keeps its wallet, logs and helper env file. Wiped on start:
# every devimint run is a new federation, so an older wallet is no use.
demo_state="${FEDIMIX_DEMO_STATE:-${XDG_CACHE_HOME:-$HOME/.cache}/fedimix-demo}/$mode"
demo_wallet="$demo_state/wallet"
demo_work="$demo_state/work"
demo_env_file="$demo_state/env.sh"
demo_pid_file="$demo_state/pid"
started_marker="=== fedimix demo environment up ==="

die() {
    echo "fedimix demo: $*" >&2
    exit 1
}

# Prints to the terminal even when devimint holds stdout (see demo_devimint).
say() {
    if { true >&3; } 2>/dev/null; then echo "$*" >&3; else echo "$*"; fi
}

# ---- Outside the integration shell ----

demo_check_host() {
    command -v nix >/dev/null 2>&1 ||
        die "nix is not installed. Install Nix with flakes enabled (https://nixos.org/download)."
    [[ -f "$root/flake.nix" ]] || die "no flake.nix in $root: run this from the Fedimix repository."
}

# Runs this script again inside `nix develop .#integration`, which has
# devimint, the fedimint daemons, bitcoind, lnd, esplora, sway and grim.
demo_enter_shell() {
    if [[ -n "${FEDIMIX_DEMO_IN_SHELL:-}" ]] && command -v devimint >/dev/null 2>&1; then
        return
    fi
    demo_check_host
    echo "entering the integration dev shell (first time: downloads fedimint from cachix)..." >&2
    export FEDIMIX_DEMO_IN_SHELL=1
    exec nix develop "$root#integration" --accept-flake-config -c "$@"
}

# ---- Inside the integration shell ----

# Every tool the environment needs, with one clear list of what is missing.
demo_check_shell() {
    local missing=() tool
    for tool in devimint fedimintd gatewayd gateway-cli fedimint-cli bitcoind bitcoin-cli \
        lnd lncli esplora jq cargo sway swaymsg grim "$@"; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    if ((${#missing[@]})); then
        die "missing from the environment: ${missing[*]}. Start it with 'just demo-live' or 'just record-demo', which enter 'nix develop .#integration'."
    fi
    [[ -n "${FEDIMIX_UI_LD_LIBRARY_PATH:-}" ]] ||
        die "FEDIMIX_UI_LD_LIBRARY_PATH is unset: this is not the integration dev shell."
}

demo_build() {
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
    echo "building fedimix (fast when nothing changed)..." >&2
    (cd "$root" && cargo build -q -p fedimix-gtk) || die "cargo build failed"
    demo_app="$CARGO_TARGET_DIR/debug/fedimix"
    export demo_app
}

# Stops an earlier run of this mode that is still up, by its own pid and test
# directory only: another devimint on this machine (another agent's tests,
# the other mode) is never touched.
demo_stop_previous() {
    local pid="" test_dir=""
    [[ -f "$demo_pid_file" ]] && pid=$(cat "$demo_pid_file")
    if [[ -f "$demo_env_file" ]]; then
        test_dir=$(sed -n 's/^declare -gx FM_TEST_DIR="\(.*\)"$/\1/p' "$demo_env_file")
    fi
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        echo "stopping the earlier $mode demo (pid $pid)..." >&2
        kill -INT -- "-$pid" 2>/dev/null || kill -INT "$pid" 2>/dev/null || true
        for _ in $(seq 1 60); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
        kill -KILL -- "-$pid" 2>/dev/null || true
    fi
    demo_kill_test_dir "$test_dir"
    rm -f "$demo_pid_file"
}

# Kills what is left of one devimint run: every process naming its directory.
demo_kill_test_dir() {
    local test_dir="$1"
    [[ -n "$test_dir" && "$test_dir" == /* && ${#test_dir} -gt 8 ]] || return 0
    pkill -f -- "$test_dir" 2>/dev/null || true
    sleep 1
    pkill -KILL -f -- "$test_dir" 2>/dev/null || true
}

demo_fresh_state() {
    rm -rf "$demo_wallet" "$demo_work" "$demo_env_file"
    mkdir -p "$demo_work"
    chmod 700 "$demo_state"
}

# usage: demo_devimint <log> <command...>
# A federation of one with the default modules (mintv2, walletv2), LND and LDK
# gateways funded with ecash, then <command> inside it. devimint lives as long
# as <command> does. Its own output goes to <log>; the command prints to the
# terminal through fd 3. devimint fails about 1 run in 5 at open-channel before
# anything starts: only that is tried again.
demo_devimint() {
    local log="$1" status tries=0
    shift
    while true; do
        status=0
        env -u FM_ENABLE_MODULE_MINT -u FM_ENABLE_MODULE_WALLET \
            -u FM_ENABLE_MODULE_MINTV2 -u FM_ENABLE_MODULE_WALLETV2 \
            FM_FED_SIZE=1 \
            devimint dev-fed --exec "$root/scripts/fund-gateways.sh" "$@" \
            3>&1 >"$log" 2>&1 || status=$?
        if [[ "$status" == 0 ]] || grep -qF "$started_marker" "$log" ||
            ! grep -q 'open-channel' "$log" || [[ $tries -ge 2 ]]; then
            return "$status"
        fi
        tries=$((tries + 1))
        echo "devimint failed at open-channel before starting; retry $tries of 2" >&2
    done
}

btc() { $FM_BTC_CLIENT -rpcwallet=default "$@"; }
mine() { btc generatetoaddress "${1:-1}" "$(btc getnewaddress)" >/dev/null; }

demo_miner_pid=""
# Saving and the federation's deposits wait on blocks.
demo_start_miner() {
    local every="$1"
    (while sleep "$every"; do mine 1 2>/dev/null || true; done) &
    demo_miner_pid=$!
}

# Writes the variables the helper commands need, so they work from any shell.
demo_write_env() {
    {
        echo "# fedimix $mode demo, regtest devimint environment. Source it or use the helpers."
        # -g: the helpers source this inside a function.
        export -p | grep -E '^declare -x (FM_[A-Z0-9_]*|PATH|FEDIMIX_UI_LD_LIBRARY_PATH|FEDIMIX_CHAIN_URL|demo_app)=' |
            sed 's/^declare -x /declare -gx /'
    } > "$demo_env_file"
}

# Pays a bolt11 invoice. LND cannot pay its own gateway's invoice and Fedimix
# picks a gateway at random, so the LDK gateway's node pays when LND cannot.
demo_pay_invoice() {
    local invoice="$1"
    if timeout 40 $FM_LNCLI payinvoice --force "$invoice" >/dev/null 2>&1; then
        echo "paid by LND"
        return 0
    fi
    if timeout 40 $FM_GWCLI_LDK lightning pay-invoice "$invoice" >/dev/null 2>&1; then
        echo "paid by the LDK gateway's node"
        return 0
    fi
    echo "neither LND nor the LDK node could pay the invoice" >&2
    return 1
}

# Sends `btc` to `address` from bitcoind's wallet and mines a block.
demo_send_onchain() {
    local address="$1" amount="${2:-0.01}" txid
    txid=$(btc -named sendtoaddress address="$address" amount="$amount")
    mine 1
    echo "sent $amount BTC to $address (txid $txid), mined 1 block"
}

# ---- Headless sway, for driving the app ----

demo_sway_pid=""
# usage: demo_start_sway <dir> <width>x<height>
demo_start_sway() {
    local dir="$1" size="$2" wl="" sock=""
    mkdir -p "$dir"
    chmod 700 "$dir"
    printf 'output * resolution %s\noutput * bg #333333 solid_color\ndefault_border none\n' \
        "$size" > "$dir/sway.conf"
    unset WAYLAND_DISPLAY SWAYSOCK DISPLAY
    export XDG_RUNTIME_DIR="$dir"
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
        sway -c "$dir/sway.conf" --unsupported-gpu >"$dir/sway.log" 2>&1 &
    demo_sway_pid=$!
    for _ in $(seq 100); do
        wl=$(cd "$dir" && ls wayland-* 2>/dev/null | grep -v '\.lock$' | head -n1 || true)
        sock=$(ls "$dir"/sway-ipc.*.sock 2>/dev/null | head -n1 || true)
        [[ -n "$wl" && -n "$sock" ]] && break
        sleep 0.1
    done
    [[ -n "$wl" && -n "$sock" ]] || die "headless sway did not start (see $dir/sway.log)"
    export WAYLAND_DISPLAY="$wl" SWAYSOCK="$sock"
}

# The headless output's name, as sway reports it (normally HEADLESS-1).
demo_output_name() {
    swaymsg -t get_outputs | jq -r '.[0].name'
}

# usage: demo_flow <drive file> <data dir> [<seconds>]
# Runs the real app on <data dir> under the current sway with
# --demo-drive <drive file>. For each `shot NAME` it calls `demo_on_shot NAME`
# (defined by the caller), then lets the app go on. Fails unless the drive
# ends with DRIVE OK and the app exits 0 with no CRITICAL or panic.
demo_flow() {
    local drive="$1" dir="$2" limit="${3:-900}"
    local name out err stdin fd pid seen=0 total line status="" code=0 deadline
    name=$(basename "$drive" .drive)
    out="$demo_work/$name.out" err="$demo_work/$name.err" stdin="$demo_work/$name.stdin"
    deadline=$((SECONDS + limit))
    : > "$out"
    rm -f "$stdin"
    mkfifo "$stdin"
    exec {fd}<>"$stdin"
    (cd "$demo_work" && LD_LIBRARY_PATH="$FEDIMIX_UI_LD_LIBRARY_PATH" \
        GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none \
        exec "$demo_app" --data-dir "$dir" --network regtest --demo-drive "$drive") \
        <&"$fd" >"$out" 2>"$err" &
    pid=$!
    demo_flow_pid=$pid
    while [[ -z "$status" ]]; do
        total=$(wc -l < "$out")
        while [[ $seen -lt $total ]]; do
            seen=$((seen + 1))
            line="$(sed -n "${seen}p" "$out")"
            case "$line" in
                "SHOT "*)
                    demo_on_shot "${line#SHOT }" || say "shot ${line#SHOT }: hook failed"
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
    demo_flow_pid=""
    exec {fd}>&-
    if [[ "$status" != "DRIVE OK" || "$code" != 0 ]] || grep -Eq 'CRITICAL|panicked' "$err"; then
        say "flow $name failed: '$status', exit $code (logs: $out, $err)"
        tail -n 20 "$out" "$err" >&2
        return 1
    fi
    say "flow $name: ok"
}

# Takes a 1280x800 screenshot of the headless output.
demo_grim() {
    sleep 0.3
    grim "$1"
}
