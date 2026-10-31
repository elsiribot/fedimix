# Fedimix

Fedimix is a Linux-first desktop application, written in Rust with GTK4 and
libadwaita, for a Fedimint-based bitcoin mixer. It is a workspace of
`fedimix-core` (logic, API types, `Backend` trait, demo backend) and
`fedimix-gtk` (the app). Phase 1 is the complete UI running over an in-memory
demo backend: the app only starts with `--demo`.

Development uses Nix. `nix develop` (or `direnv allow`, via the bundled
`.envrc`) gives you a shell with the stable Rust toolchain (including clippy,
rustfmt and rust-analyzer), GTK4, libadwaita and `just`, plus sway, grim and jq for headless screenshots. From there:

```sh
nix develop
just run      # build and launch the app (the demo, for now)
just test     # run the workspace tests
just screenshot NAME [--demo-scenario S] [--demo-page P]  # headless, to target/screenshots/NAME.png
just screenshot-all   # every scenario x page at 480 and 360 wide
just interact-demo    # scripted UI flows under headless sway
just clippy   # lint with warnings denied
just fmt      # format the code
```

`just test-integration` runs the smoke tests in `crates/fedimix-core/tests/devimint.rs`
against a local devimint federation, once with the default `mintv2`/`walletv2`
modules and once with `mint`/`wallet`. It uses the separate `integration`
shell (`nix develop .#integration`), which adds devimint, the fedimint daemons,
bitcoind, lnd and esplora. Plain `nix develop` never fetches fedimint. The
`--accept-flake-config` flag in the recipe makes nix use the fedimint cachix
cache; without it nix builds fedimint from source (hundreds of crates).

`nix build` produces a wrapped release binary at `result/bin/fedimix`;
`just build-nix` does that and runs the flake checks (clippy, fmt, tests).

## Demo mode

```sh
just demo                                   # funded scenario, Saving tab
cargo run -p fedimix-gtk -- --demo --demo-scenario below-target --demo-page refill
```

Scenarios (`--demo-scenario`): `funded` (default), `below-target`,
`no-saving`, `first-run` (Join page, no federation), `offline`,
`exit-deferred` (Spending too low for exit fees, every queue and history
status), `long-names` (below target with a queue, oversized names),
`device-disconnected` (Saving set up, device unplugged), `preview-expiry`
(previews expire within a second), `device-rejects` (the device refuses).

Pages (`--demo-page`): `saving`, `spending`, `coins`, `coin-detail`, `moving`,
`notes`, `send`, `send-review`, `receive`, `refill`, `refill-expanded`, `join`,
`setup-saving`, `settings`, `about`.

## Screenshots and scripted flows

`scripts/screenshot-all.sh` (`just screenshot-all`) renders every sensible
scenario and page at 480x640 and 360x640 (`-narrow`) into
`target/screenshots/<scenario>-<page>[-narrow].png`. It fails if a window never
maps or the app logs `CRITICAL` or `panicked`.

`--demo-drive <file>` (requires `--demo`) replays a script inside the running
app, because OS-level input injection does not reach GTK under headless sway.
The steps are documented at the top of `crates/fedimix-gtk/src/demo_drive.rs`.
It prints `DRIVE OK`, or
`DRIVE FAILED: <step>` and exits 1. `scripts/interact-demo.sh` (`just
interact-demo [flow...]`) runs `scripts/demo-flows/*.drive` under headless
sway and takes a grim screenshot per `shot` into `target/screenshots/interact/`.
A flow's `# scenario:`, `# page:` and `# size:` header comments pick its
scenario, start page and window size.
