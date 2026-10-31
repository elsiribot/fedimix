default:
    @just --list

build:
    cargo build --workspace

# Phase 1 only runs on the demo backend
run: demo

demo:
    cargo run -p fedimix-gtk -- --demo

test:
    cargo test --workspace

# Smoke tests against devimint federations, once per module generation (slow)
test-integration:
    nix develop .#integration --accept-flake-config -c scripts/test-integration.sh

check:
    cargo check --workspace --all-targets

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

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
