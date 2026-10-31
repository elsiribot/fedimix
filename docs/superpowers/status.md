# Status

Phase 1 is complete: Tasks 1-12 and the final review's fix list are done. Phase 2 Tasks 1-9 are done: fedimint 0.13, store, seed and lock, `RealBackend` with receive and send, and, since Task 9, `fedimix` runs on the real backend without `--demo`. Saving stays a later phase: its empty state says so, and Setup is off.

## What works (commands run in `nix develop`)
- `just clippy`, `just test` (includes `tests/real_backend.rs`: `RealBackend` over the fake port).
- `nix build` (fails if the binary cannot load its libraries; `result/bin/fedimix --demo` was started once under headless sway with a clean environment), or `just build-nix` for that plus `nix flake check` (clippy, fmt, tests on the same source).
- `just test-integration` (slow; devimint with both module sets, 24 tests each: `RealBackend` restart, receive over Lightning and on-chain, send over Lightning and on-chain, a refunded payment, an expired invoice, paying your own invoice, a send killed right after submission, a deposit across a killed backend, recovery of a lost client database, join and reopen, deposit, too-small deposit, Lightning receive and pay, changed gateway terms, withdraw, quote errors, finding an operation by intent, minimum deposit, bad invites, the smoke test; `scripts/fund-gateways.sh` gives every gateway ecash first; a generation is retried at most twice if devimint fails at `open-channel` before any test). Uses `nix develop .#integration --accept-flake-config`; plain `nix develop` does not touch fedimint.
- `just test-real-ui` (slow; the real binary on devimint, default modules, under headless sway; a devimint start that fails at `open-channel` is retried at most twice): Join with `FM_INVITE_CODE`, the Saving empty state, an invoice LND pays, the words prompt, a restart showing federation and balance, a second launch reporting the lock and exiting 1, a Lightning payment to a second wallet confirmed `Claimed` by that wallet, and the folder refusing `--network signet` with exit 1. Screenshots in `target/screenshots/real/`.
- `just run [--data-dir PATH] [--network bitcoin|signet|regtest]`: the real wallet. Default folders: `$XDG_DATA_HOME/fedimix` (else `~/.local/share/fedimix`), `fedimix-signet`, `fedimix-regtest`.
- `just demo`; `--demo-scenario funded|below-target|no-saving|first-run|offline|exit-deferred|long-names|device-disconnected|preview-expiry|device-rejects|send-stuck|long-history|recovering`, `--demo-page ...`.
- Core API is the Phase 2 shape: operations with state events and cancel, expiring previews, exit quotes, `parse_destination`, device identity, queue deferrals.

## Verified headlessly (sway + grim, GTK_A11Y=none, cairo renderer)
- `just interact-demo`: 24 scripted flows (21: history "Show more" on both tabs at 360 px; 22: recovery words, off the clipboard after 60 s; 23: something copied later stays; 24: the restoring banner) through `--demo-drive`, with assertions on outcomes. Removing the Refill guard, the Send lock, the single window or the grace-period refusal made its flow fail.
- `just screenshot-all`: every listed page at 480x640 and 360x640 with no CRITICAL or panic. `long-names` and `device-disconnected` were read by eye at 360 px; nothing overflows.

## Not verified
- Failed `set_active_federation` reverting the radio: the demo has one federation, so nothing can fail. No flow reaches it.
- The loading spinner and the error page with Retry for a failed first snapshot: the demo never fails to load, so no flow reaches them.
- A panic in a core future: covered by a unit test of `CoreBridge::call`, not through the UI.
- Real pointer/keyboard input (the driver emits clicks and sets text), Escape as a key, the Apply button's own click (the driver emits `apply`), the header menu popover (opened through actions), Enter in entries.
- Dark theme, accessibility, a real desktop (sway only), hardware. A second process launch is tested only with no session bus; `NON_UNIQUE` is what should make it the same on a desktop.
- Mainnet and signet: every real run was regtest.
- The clipboard: flows read it from inside the app, where GTK answers locally. No other Wayland client was tried, and a clipboard manager can keep its own copy, which Fedimix cannot clear: that is what the toast is for. If Fedimix quits inside the 60 s, what happens to the clipboard was not observed.
- A real recovery on screen: the state is tested over the fake port (`tests/real_backend.rs`), the banner through the demo's `recovering`, and `devimint.rs` covers recovery itself. During a recovery Send and Receive answer "You are offline".
- The window hiding at shutdown: every flow quits through that path and exits 0, but nothing looks at the window while it happens. A shutdown that takes the whole 30 s was not run in the UI.
- A real on-chain receive or send through the UI: `tests/devimint.rs` covers them below the UI; the UI flow is Lightning only.
- `parse_destination` is lexical (base58 by shape); `validate_address` checks checksum and network, and `RealBackend::prepare_send` calls it.
- Window close and Quit wait for `shutdown` (30 s cap): unit-tested cap, exercised by every flow ending through the quit action; a hung backend is not run in the UI.
- Startup `operations()` watchers: the demo never has a running operation at start, so only the unit test of `resumed_outcome` covers them.

## Next
Saving, in a later phase. Still open from the re-review: O1-O4 (not in Task 1) and N8 (accepted).

## Needs user
- SOCKS5 for federation traffic: fedimint 0.13 connectors have no such option, so a set proxy keeps federations offline.
  - A proxy set while running closes clients, but connectors already built stay until restart, and Settings says so. See `decisions.md`.
