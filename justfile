default:
    @just --list

build:
    cargo build --workspace

# The real wallet; --data-dir and --network go to the app
run *args:
    cargo run -p fedimix-gtk -- {{args}}

demo:
    cargo run -p fedimix-gtk -- --demo

test:
    cargo test --workspace
    scripts/check-no-soft-signer.sh

# Smoke tests against devimint federations, once per module generation (slow)
test-integration:
    nix develop .#integration --accept-flake-config -c scripts/test-integration.sh

# The real binary through Join, receive, send and restart, on devimint under headless sway (slow)
test-real-ui:
    nix develop .#integration --accept-flake-config -c scripts/test-real-ui.sh

check:
    cargo check --workspace --all-targets

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

# Live demo on a local regtest devimint: pre-funded wallet in a real window; Ctrl-C stops it.
# Helpers while it runs: just demo-live pay <invoice> | send <address> [BTC] | mine [N] | open | info
demo-live *args:
    scripts/demo-live.sh {{args}}

# Record the Phase 3 story headlessly on regtest to ~/fedimix-demo/phase3 (video and screenshots)
record-demo:
    scripts/record-demo.sh

# Build the release package and run the flake checks (clippy, fmt, tests)
build-nix:
    nix build
    nix flake check

# Render the app headlessly to target/screenshots/<name>.png; args go to the app after --demo
screenshot name *args:
    scripts/screenshot.sh target/screenshots/{{name}}.png -- --demo {{args}}

# Screenshot every scenario x page at 480 and 360 wide into target/screenshots
screenshot-all:
    scripts/screenshot-all.sh

# Run the scripted UI flows (scripts/demo-flows/*.drive) headlessly
interact-demo *flows:
    scripts/interact-demo.sh {{flows}}
