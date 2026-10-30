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

# Render the app headlessly to target/screenshots/<name>.png
screenshot name *args:
    scripts/screenshot.sh target/screenshots/{{name}}.png -- {{args}}
