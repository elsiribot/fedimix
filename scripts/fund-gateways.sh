#!/usr/bin/env bash
# Runs inside `devimint dev-fed --exec`: pegs in each vetted gateway that holds no
# ecash, because dev-fed funds only two of the three and an empty gateway fails
# every receive routed to it. Then runs the command it was given.
set -euo pipefail

gateway_cli="${FM_GWCLI_LDK%% -a *}"
federation=$($FM_MINT_CLIENT info | jq -r .federation_id)
btc() { $FM_BTC_CLIENT -rpcwallet=default "$@"; }
mine() { btc generatetoaddress "$1" "$(btc getnewaddress)" >/dev/null; }
ecash() { $gateway_cli -a "$1" get-balances | jq -r '.ecash_balances[0].ecash_balance_msats'; }

for gateway in $($FM_MINT_CLIENT module lnv2 gateways list | jq -r '.[]'); do
    [ "$(ecash "$gateway")" = 0 ] || continue
    address=$($gateway_cli -a "$gateway" ecash pegin --federation-id "$federation" |
        jq -r 'if type == "object" then .address else . end')
    btc -named sendtoaddress address="$address" amount=0.01 >/dev/null
    mine 12
    for _ in $(seq 1 60); do
        [ "$(ecash "$gateway")" = 0 ] || break
        sleep 2
        mine 1
    done
    if [ "$(ecash "$gateway")" = 0 ]; then
        echo "gateway $gateway never received its peg-in" >&2
        exit 1
    fi
done

exec "$@"
