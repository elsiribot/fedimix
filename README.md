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
just run      # build and launch the app
just test     # run the workspace tests
just screenshot NAME  # headless screenshot to target/screenshots/NAME.png
just screenshot-all   # every scenario x page at 480 and 360 wide
just interact-demo    # scripted UI flows under headless sway
just clippy   # lint with warnings denied
just fmt      # format the code
```

`nix build` produces a wrapped release binary at `result/bin/fedimix`.

## Demo mode

```sh
just demo                                   # funded scenario, Saving tab
cargo run -p fedimix-gtk -- --demo --demo-scenario below-target --demo-page refill
```

Scenarios (`--demo-scenario`): `funded` (default), `below-target`,
`no-saving`, `first-run` (Join page, no federation), `offline`,
`exit-deferred` (Spending too low for exit fees, every queue and history status).

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
One step per line: `click <label>`, `type <title> <text>`, `expect <text>`,
`wait <ms>`, `shot <name>`, `expect-not <text>` and
`action <name>` (e.g. `app.settings`). It prints `DRIVE OK`, or
`DRIVE FAILED: <step>` and exits 1. `scripts/interact-demo.sh` (`just
interact-demo [flow...]`) runs `scripts/demo-flows/*.drive` under headless
sway and takes a grim screenshot per `shot` into `target/screenshots/interact/`.
A flow's `# scenario:` and `# size:` header comments pick its scenario and
window size.
