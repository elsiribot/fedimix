# Fedimix

Fedimix is a Linux-first desktop application, written in Rust with GTK4 and
libadwaita, for a Fedimint-based bitcoin mixer. It is a workspace of
`fedimix-core` (logic, API types, `Backend` trait, demo backend) and
`fedimix-gtk` (the app). `fedimix` runs on the real backend: Spending holds
ecash in a Fedimint federation you join with an invite code, over Lightning and
on-chain. Saving, the cold-storage side, is on-chain bitcoin kept on a hardware
wallet (Ledger, Coldcard, Jade or BitBox02) or, without one, on this computer;
Refill moves it into Spending. Hardware signing has never been run on a real
device. `--demo` shows the complete UI over an in-memory demo backend.

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

## Saving

Saving holds on-chain bitcoin that only a device you control can spend. Set it
up once per data folder, from the Saving tab. Receive gives a fresh address each
time; Send pays any address, with change going to your active federation; Refill
moves Saving's coins into Spending. Every spend is reviewed in the app, then
confirmed on the device. Saving needs a chain server (Esplora or Electrum,
chosen in Settings); on signet it uses 1 sat/vB when the server has no fee
estimate, on bitcoin it refuses to send until the server has one.

### Hardware wallets

Supported: Ledger (Bitcoin app open), Coldcard, Jade and BitBox02 (first
connection pairs with a code shown on the device and the computer). Trezor is
not supported: the library Fedimix uses for devices, async-hwi, has none.

The kernel must let your user talk to the device. Install the vendor's udev
rules and re-plug the device (on NixOS, add the rules package to
`services.udev.packages`):

- Ledger: `20-hw1.rules` from LedgerHQ's `udev-rules` (vendor 2c97).
- Coldcard: `51-coinkite.rules` (vendor d13e).
- Jade: `51-jade.rules` from Blockstream's Jade repository (USB serial chips).
- BitBox02: `50-shiftcrypto-bitbox02.rules` (vendor 03eb, product 2403).

These names are from the vendors' documentation and were not tried here. The
Nix package of Fedimix carries libudev, which the app needs at run time; other
packaging must declare it.

Fedimix cannot make the device show a receive address for verification: plain
BIP84 addresses cannot be displayed through async-hwi, so the Receive dialog
says "Your device cannot display this address." Check a first receive with a
small amount, or compare the address in the device's own app.

### This computer

Without a hardware wallet, Saving can be a software wallet on this computer,
listed after any hardware wallet as "This computer". Its key comes from the same
recovery words as Spending (BIP84, account `m/84'/0'/0'` on bitcoin,
`m/84'/1'/0'` on test networks, no passphrase), so one phrase restores both
wallets. It signs only when you confirm a send or a refill, and it talks to no
server, so a proxy does not affect it.

A hardware wallet is safer for large amounts: with the software wallet, anyone
who can read the `seed` file can spend Saving too, not just Spending. Choose
with care: Saving is set up once per data folder, and Fedimix cannot yet switch
it to a hardware wallet. To move later, send Saving's coins from Saving to an
address of the hardware wallet.

## Exits: Spending to Saving

Spending keeps a target (default 0.05 BTC). Money above it is booked into a
queue in denominations of the 1-2-5 series, from 1 BTC down to 0.001 BTC
(Spending's "moving to Saving" link opens the queue). Each queued note leaves
for Saving's next address on its own, or early by hand.

- Anonymity estimate, two numbers. A note's own estimate counts other
  people's deposits of at least its denomination in every synced session after
  its own deposit (no window). Our own on-chain deposits are left out and
  our own Lightning receives are subtracted. A queue row reads "current to
  expected": the expected figure adds the deposits the federation's recent
  rate predicts before the note's exit. A federation's estimate, shown in
  Settings as "anon ~N" ("Most private" on the strictly largest), counts
  deposits of at least 0.001 BTC in its newest 480 synced sessions. Neither
  is a guarantee: they count deposits the exit could be mistaken for.
- Wait: a note must first sit 24 h in the pool (Settings, "Minimum hours in
  pool"). Then its exit time is drawn from an exponential distribution (a
  Poisson process) whose mean is `3 h x (anonymity target / estimate)^2`,
  never below 3 h and never above 30 days (the estimate counts as at least 1).
  The default target is 50: an estimate of 50 or more waits 3 h on average,
  25 waits 12 h, 5 waits 300 h (12.5 days), and 3 or fewer the 30-day cap.
  One automatic exit per federation at most, and at least 1 h apart.
- Nothing leaves while offline, within the grace period (default 10 min after
  connecting), when the fee exceeds the fee guard (default 2% of the note), or
  when Spending's remaining balance cannot pay the fee. The queue row says
  which.
- Early exit: "Exit now (lower privacy)" shows the fee and the anonymity set
  the note leaves with. The fee guard does not apply; the grace period does.
- Labels: Saving's coins read "From Spending". An early exit is tagged "early
  exit"; an automatic one carries its "anon set N" at the time it left. Both show
  on the Coins page and a coin's page.
- A withdrawal whose outcome cannot be told stays "outcome unknown"; the app
  cannot clear it yet.

On the real backend the local devimint has no other depositors, so the
estimate reads 0 in every real run here (the queue and exits still work). Try
them on regtest with fast timing (a testing build; refused on bitcoin):

```sh
just test-real-ui                 # includes scripts/real-flows/exits.drive
just record-exits                 # records them to ~/fedimix-demo/phase4/real-exits/exits.mp4
just record-exits-demo            # the simulated exits on the demo backend (phase4/exits-demo.mp4)
just demo                         # the simulated queue; or: cargo run -p fedimix-gtk -- --demo --demo-scenario exits
```

## Not yet

- Cover traffic: its controls are hidden in the real wallet.
- Trezor.
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
