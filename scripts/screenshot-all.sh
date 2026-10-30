#!/usr/bin/env bash
# Screenshot every scenario x page combination that makes sense, at 480x640
# and 360x640 (-narrow), into target/screenshots/<scenario>-<page>[-narrow].png.
# Fails if any window never maps or the app logs CRITICAL/panicked.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/target/screenshots"
mkdir -p "$out"
(cd "$root" && cargo build -q -p fedimix-gtk)

# scenario:page list; page "start" means no --demo-page (the plain window).
funded_pages="saving spending coins coin-detail moving notes send send-review receive refill refill-expanded join setup-saving settings about"
jobs_file="$(mktemp)"
trap 'rm -f "$jobs_file"' EXIT
add() { # scenario page
    local scenario="$1" page="$2" args=(--demo --demo-scenario "$1")
    [[ "$page" != start ]] && args+=(--demo-page "$page")
    for size in "480 " "360 -narrow"; do
        local w="${size%% *}" suffix="${size#* }"
        printf '%s\t%s\t%s\n' "$out/$scenario-$page$suffix.png" "$w" "${args[*]}" >> "$jobs_file"
    done
}
for p in $funded_pages; do add funded "$p"; done
for p in spending refill refill-expanded; do add below-target "$p"; done
for p in saving spending setup-saving; do add no-saving "$p"; done
add offline spending
add first-run start
add first-run join

run_job() {
    IFS=$'\t' read -r png width args <<<"$1"
    # shellcheck disable=SC2086
    "$root/scripts/screenshot.sh" "$png" --width "$width" --height 640 -- $args >/dev/null \
        || { echo "FAILED: $png ($args)" >&2; return 1; }
    echo "ok $png"
}
export -f run_job
export root
fail=0
# Each job gets its own sway session, so they can run side by side.
xargs -d '\n' -P 4 -I{} bash -c 'run_job "$1"' _ {} < "$jobs_file" || fail=1
exit $fail
