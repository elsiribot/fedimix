#!/usr/bin/env bash
# Runs the devimint tests against both module generations (mint/wallet and
# mintv2/walletv2). Needs the `integration` dev shell; see `just test-integration`.
set -euo pipefail
cd "$(dirname "$0")/.."

run() {
    echo "=== devimint: $1 ==="
    shift
    env FM_FED_SIZE=1 "$@" devimint dev-fed --exec \
        cargo test -p fedimix-core --test devimint -- --ignored --test-threads=1
}

run "default modules (mintv2, walletv2)"
run "v1 modules (mint, wallet)" \
    FM_ENABLE_MODULE_MINT=1 FM_ENABLE_MODULE_WALLET=1 \
    FM_ENABLE_MODULE_MINTV2=0 FM_ENABLE_MODULE_WALLETV2=0
