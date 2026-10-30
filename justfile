default:
    @just --list

build:
    cargo build --workspace

run:
    cargo run -p fedimix-gtk

demo:
    cargo run -p fedimix-gtk -- --demo

test:
    cargo test --workspace

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

# Render the app headlessly to target/screenshots/<name>.png
screenshot name *args:
    scripts/screenshot.sh target/screenshots/{{name}}.png -- {{args}}

# Screenshot every scenario x page at 480 and 360 wide into target/screenshots
screenshot-all:
    scripts/screenshot-all.sh

# Run the scripted UI flows (scripts/demo-flows/*.drive) headlessly
interact-demo *flows:
    scripts/interact-demo.sh {{flows}}
