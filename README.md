# Fedimix

Fedimix is a Linux-first desktop application, written in Rust with GTK4 and
libadwaita, for a Fedimint-based bitcoin mixer. It is a workspace of
`fedimix-core` (logic, API types, `Backend` trait, demo backend) and
`fedimix-gtk` (the app). `fedimix` runs on the real backend: Spending holds
ecash in a Fedimint federation you join with an invite code, over Lightning and
on-chain. Saving, the cold-storage side, arrives in a later version; `--demo`
shows the complete UI over an in-memory demo backend.

Development uses Nix. `nix develop` (or `direnv allow`, via the bundled
`.envrc`) gives you a shell with the stable Rust toolchain (including clippy,
rustfmt and rust-analyzer), GTK4, libadwaita and `just`, plus sway, grim and jq for headless screenshots. From there:

```sh
nix develop
just run      # build and launch the wallet; arguments go to the app
just test     # run the workspace tests
just screenshot NAME [--demo-scenario S] [--demo-page P]  # headless, to target/screenshots/NAME.png
just screenshot-all   # every scenario x page at 480 and 360 wide
just interact-demo    # scripted UI flows under headless sway
just clippy   # lint with warnings denied
just fmt      # format the code
```

`just test-integration` runs the tests in `crates/fedimix-core/tests/devimint.rs`
against a local devimint federation, once with the default `mintv2`/`walletv2`
modules and once with `mint`/`wallet`. It uses the separate `integration`
shell (`nix develop .#integration`), which adds devimint, the fedimint daemons,
bitcoind, lnd and esplora. Plain `nix develop` never fetches fedimint. The
`--accept-flake-config` flag in the recipe makes nix use the fedimint cachix
cache; without it nix builds fedimint from source (hundreds of crates).

`just test-real-ui` drives the real binary under headless sway, on a devimint
federation with the default modules: Join, a Lightning receive from LND, a
restart, a second launch on the same folder, a Lightning payment to a second
wallet, and a folder refusing another network. Screenshots land in
`target/screenshots/real/`.

`nix build` produces a wrapped release binary at `result/bin/fedimix`;
`just build-nix` does that and runs the flake checks (clippy, fmt, tests).

## Running the wallet

```sh
fedimix [--data-dir PATH] [--network bitcoin|signet|regtest]
```

The network defaults to `bitcoin`: this spends real money. Try it with small
amounts first.

Each network has its own data folder, so test networks never collide:
`$XDG_DATA_HOME/fedimix` (else `~/.local/share/fedimix`) for bitcoin,
`fedimix-signet` and `fedimix-regtest` next to it. A folder remembers its
network and refuses another one, also through `--data-dir`. If the wallet
cannot start, for example because another Fedimix holds the folder, it says why
and exits 1.

The first launch creates a seed in the folder (`seed`, mode 0600, a plain file,
not encrypted). Whoever can read it can spend your funds. The folder is created
0700 and Fedimix refuses a seed or folder open to others. Spending asks you to
write down the recovery words once you hold funds, and Settings shows them at
any time.

In this version the words alone do not restore a wallet in the app, and they do
not say which federations you joined. Keep your invite codes with them.

Never run or restore an old copy of the data folder, and never run one folder on
two machines: that reuses ecash nonces, and ecash spent twice is lost. Deleting
`seed` does not make a wallet safer either: Fedimix then refuses to start until
the file is back.

To restore by hand on a new device:

1. Create a new, empty data folder with mode 0700. Do not copy the old one.
2. Write the words, separated by spaces, into a file named `seed` in that
   folder, with mode 0600.
3. Start Fedimix with `--data-dir` on that folder.
4. Join each federation with its invite code. Fedimix rebuilds the balance from
   the federation, which can take a while; the old history does not come back.
5. If Saving was the software wallet, set it up again with "This computer": the
   same words give the same Saving account, and its first scan finds the coins.

## Saving without a hardware wallet

Saving is meant for a hardware wallet (Ledger, Coldcard, Jade, BitBox02). If you
have none, Saving can be a software wallet on this computer, listed after any
hardware wallet as "This computer". Its key comes from the same recovery words
as Spending (BIP84, account `m/84'/0'/0'` on bitcoin, `m/84'/1'/0'` on test
networks, no passphrase), so one phrase restores both wallets. It signs only
when you confirm a send or a refill, and it talks to no server, so a proxy
does not affect it.

A hardware wallet is safer for large amounts: with the software wallet, anyone
who can read the `seed` file can spend Saving too, not just Spending. Choose
with care: Saving is set up once per data folder, and Fedimix cannot yet switch
it to a hardware wallet. To move later, send Saving's coins from Saving to an
address of the hardware wallet.

## Not yet

- Saving (the cold-storage side) and hardware wallets: the controls are hidden
  in the real wallet.
- Exits and cover traffic.
- A proxy for federation traffic: fedimint has no SOCKS5 support, so setting a
  proxy keeps federations offline instead of connecting directly.
- Restoring from the recovery words inside the app; see the manual steps above.
- Switching Saving from the software wallet to a hardware wallet.

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
(previews expire within a second), `device-rejects` (the device refuses), `confirm-fails` (Spending sends refused
at Confirm),
`recovering` (data being restored from the federation).

Pages (`--demo-page`): `saving`, `spending`, `coins`, `coin-detail`, `moving`,
`notes`, `send`, `send-review`, `receive`, `refill`, `refill-expanded`, `join`,
`setup-saving`, `settings`, `about`.

## Screenshots and scripted flows

`scripts/screenshot-all.sh` (`just screenshot-all`) renders every sensible
scenario and page at 480x640 and 360x640 (`-narrow`) into
`target/screenshots/<scenario>-<page>[-narrow].png`. It fails if a window never
maps or the app logs `CRITICAL` or `panicked`.

`--demo-drive <file>` replays a script inside the running app, demo or real but
never on `--network bitcoin`, because OS-level input injection does not reach GTK under headless sway.
The steps are documented at the top of `crates/fedimix-gtk/src/demo_drive.rs`.
It prints `DRIVE OK`, or
`DRIVE FAILED: <step>` and exits 1. `scripts/interact-demo.sh` (`just
interact-demo [flow...]`) runs `scripts/demo-flows/*.drive` under headless
sway and takes a grim screenshot per `shot` into `target/screenshots/interact/`.
A flow's `# scenario:`, `# page:` and `# size:` header comments pick its
scenario, start page and window size.
