default:
    @just --list

build:
    cargo build

run:
    cargo run

check:
    cargo check --all-targets

clippy:
    cargo clippy --all-targets -- -D warnings

fmt:
    cargo fmt --all
