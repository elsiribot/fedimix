#!/usr/bin/env bash
# Proves the `testing` signer is not in a normal build of the app: the
# `testing` feature of fedimix-core, which alone compiles `SoftSigner` with its
# reject hooks and the fakes, must be off in fedimix-gtk's normal and build
# dependency graph. `SeedSigner`, the software Saving wallet keyed from the
# recovery words, ships in every build on purpose and is not what this checks.
# It fails when it cannot tell: if cargo fails, or the graph does not show
# fedimix-core at all, or a graph that is known to enable `testing`
# (fedimix-core's own tests) does not show it.
set -euo pipefail
cd "$(dirname "$0")/.."

tree() {
    cargo tree --offline --prefix none -f '{p} [{f}]' "$@"
}

# What a release build of the app links, with its features.
app="$(tree -p fedimix-gtk -e normal,build)" || {
    echo "check-no-soft-signer: cargo tree failed for fedimix-gtk" >&2
    exit 1
}
core_line="$(grep -E '^fedimix-core v' <<<"$app" | sort -u || true)"
if [[ -z "$core_line" ]]; then
    echo "check-no-soft-signer: fedimix-core is missing from the fedimix-gtk graph; cannot check" >&2
    exit 1
fi

# The control: fedimix-core's own test build turns `testing` on, and the same
# command must show it, or the command shows nothing.
control="$(tree -p fedimix-core -e normal,dev)" || {
    echo "check-no-soft-signer: cargo tree failed for fedimix-core" >&2
    exit 1
}
if ! grep -E '^fedimix-core v.*\[.*\btesting\b' <<<"$control" >/dev/null; then
    echo "check-no-soft-signer: the control graph does not show the testing feature; cannot check" >&2
    exit 1
fi

if grep -E '\btesting\b' <<<"$core_line" >/dev/null; then
    echo "check-no-soft-signer: FAIL, the app's build enables fedimix-core/testing:" >&2
    echo "$core_line" >&2
    exit 1
fi
echo "check-no-soft-signer: ok, fedimix-gtk builds fedimix-core without the testing feature"
