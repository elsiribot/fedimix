# Status

Phase 1, Tasks 1-10 done (workspace split/dev shell/screenshot harness; core API types, events, Backend trait, Fedimix handle; DemoBackend; gtk format/present logic; app shell with CLI, CoreBridge, AppContext and window; Saving tab with shared widgets and history list; Spending tab, Coins/coin detail/Notes pages; Moving to Saving page; Send and Receive dialogs; Refill dialog).

## What works
- `nix develop -c just build|test|clippy|fmt` on the `fedimix-core` + `fedimix-gtk` workspace.
- `nix develop -c just run` launches the placeholder window.
- `nix develop -c just screenshot <name> [args...]` renders the app headlessly
  (sway + grim) to `target/screenshots/<name>.png`.

- `fedimix-core` exposes `Amount`, state/preview types, `Event`/`EventStream`, `FedimixError`, `Backend` (async_trait) and the `Fedimix` forwarding handle; `DemoBackend::new(DemoScenario)` (funded, below-target, no-saving, first-run, offline) implements Backend with mockup data; 21 unit tests via `nix develop -c just test`.

- `fedimix-gtk` has pure `format` (btc/date/parse) and `present` (captions, tags, history/queue rows, send validation) modules, 23 unit tests; crate-level `allow(dead_code)` until Task 12.

- `nix develop -c just demo` (or `scripts/screenshot.sh out.png [--width 400] -- --demo [--demo-scenario <name>]`) shows the shell: Wide header switcher, bottom switcher bar below 500sp, placeholder tabs; `--demo-page` is parsed only. Without `--demo` the app exits 2.

- The Saving tab shows device, balance, depositing link, Send/Receive (dialogs), and history with `--demo`; with `--demo-scenario no-saving` its empty state offers "Set up" (toast). Its balance opens the Coins page (rows open coin detail with a Freeze switch).

- The Spending tab shows Offline and "Set up Saving" banners, balance (opens Notes page), bar and caption, "Refill from Saving", Send/Receive and history with a "Show cover traffic" toggle. Refill and Send/Receive open dialogs; Set up is still a toast. The Moving queue link opens the Moving to Saving page (live rows, level bars, row menu "Exit now (lower privacy)" with confirm dialog; insensitive while waiting for Saving). `--demo-page saving|spending|coins|coin-detail|notes|moving|send|send-review|receive|refill|refill-expanded` work; other pages are ignored until their tasks.

- Send dialog (recipient+amount, Review page with Confirm / "Confirm on <device>" / "Connect your <device>") and Receive dialog (Lightning + On-chain for Spending, direct address for Saving) with QR and Copy.

- Refill dialog ("Refill from Saving"): calls prepare_refill (error = toast, no dialog), shows amount with a chevron toggling the coin list and overshoot note, Confirm on <device> (or disabled "Connect your <device>"), outcome toast.

## Next
Task 11.

## Needs user
Nothing.
