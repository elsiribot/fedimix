# Status

Phase 1, Tasks 1-7 done (workspace split/dev shell/screenshot harness; core API types, events, Backend trait, Fedimix handle; DemoBackend; gtk format/present logic; app shell with CLI, CoreBridge, AppContext and window; Saving tab with shared widgets and history list; Spending tab, Coins/coin detail/Notes pages).

## What works
- `nix develop -c just build|test|clippy|fmt` on the `fedimix-core` + `fedimix-gtk` workspace.
- `nix develop -c just run` launches the placeholder window.
- `nix develop -c just screenshot <name> [args...]` renders the app headlessly
  (sway + grim) to `target/screenshots/<name>.png`.

- `fedimix-core` exposes `Amount`, state/preview types, `Event`/`EventStream`, `FedimixError`, `Backend` (async_trait) and the `Fedimix` forwarding handle; `DemoBackend::new(DemoScenario)` (funded, below-target, no-saving, first-run, offline) implements Backend with mockup data; 21 unit tests via `nix develop -c just test`.

- `fedimix-gtk` has pure `format` (btc/date/parse) and `present` (captions, tags, history/queue rows, send validation) modules, 23 unit tests; crate-level `allow(dead_code)` until Task 12.

- `nix develop -c just demo` (or `scripts/screenshot.sh out.png [--width 400] -- --demo [--demo-scenario <name>]`) shows the shell: Wide header switcher, bottom switcher bar below 500sp, placeholder tabs; `--demo-page` is parsed only. Without `--demo` the app exits 2.

- The Saving tab shows device, balance, depositing link, Send/Receive (toast "Not available yet"), and history with `--demo`; with `--demo-scenario no-saving` its empty state offers "Set up" (toast). Its balance opens the Coins page (rows open coin detail with a Freeze switch).

- The Spending tab shows Offline and "Set up Saving" banners, balance (opens Notes page), bar and caption, "Refill from Saving", Send/Receive and history with a "Show cover traffic" toggle. Moving queue link, Refill, Set up, Send/Receive toast "Not available yet". `--demo-page saving|spending|coins|coin-detail|notes` works; other pages are ignored until their tasks.

## Next
Task 8.

## Needs user
Nothing.
