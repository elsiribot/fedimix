#!/usr/bin/env bash
# Runs the devimint tests against both module generations (mint/wallet and
# mintv2/walletv2). Needs the `integration` dev shell; see `just test-integration`.
set -euo pipefail
cd "$(dirname "$0")/.."

out="$(mktemp)"
trap 'rm -f "$out"' EXIT

# How many times a generation is started again after devimint failed to start.
max_retries=2

# devimint fails about 1 run in 5 at `gateway-cli ... open-channel`, before any
# test runs. Only that is worth another try: once tests have started, a failure
# is theirs, also one that killed the test binary before its summary.
failed_at_startup() {
    ! grep -q 'test result:' "$out" &&
        ! grep -Eq '^running [0-9]+ tests?$' "$out" &&
        grep -q 'open-channel' "$out"
}

# usage: run <label> <v1|v2> [VAR=value ...]
run() {
    local label="$1" gen="$2" retries=0 status
    shift 2
    while true; do
        echo "=== devimint: $label ==="
        status=0
        # Only the test process gets the C++ runtime; devimint's daemons must not.
        env -u FM_ENABLE_MODULE_MINT -u FM_ENABLE_MODULE_WALLET \
            -u FM_ENABLE_MODULE_MINTV2 -u FM_ENABLE_MODULE_WALLETV2 \
            FM_FED_SIZE=1 "$@" \
            devimint dev-fed --exec \
            scripts/fund-gateways.sh \
            env "LD_LIBRARY_PATH=${FEDIMIX_TEST_LD_LIBRARY_PATH:-}" FEDIMIX_EXPECT_MODULES="$gen" \
            cargo test -p fedimix-core --test devimint -- --ignored --test-threads=1 \
            2>&1 | tee "$out" || status=$?
        if [ "$status" = 0 ]; then
            break
        fi
        if [ "$retries" -ge "$max_retries" ] || ! failed_at_startup; then
            exit "$status"
        fi
        retries=$((retries + 1))
        echo "devimint failed at open-channel before any test; retry $retries of $max_retries" >&2
    done
    # A run that matched no tests exits 0 too, so require a real pass.
    if ! grep -Eq '^test result: ok\. [1-9][0-9]* passed' "$out"; then
        echo "no devimint test passed in the '$label' run" >&2
        exit 1
    fi
}

run "default modules (mintv2, walletv2)" v2
run "v1 modules (mint, wallet)" v1 \
    FM_ENABLE_MODULE_MINT=1 FM_ENABLE_MODULE_WALLET=1 \
    FM_ENABLE_MODULE_MINTV2=0 FM_ENABLE_MODULE_WALLETV2=0
