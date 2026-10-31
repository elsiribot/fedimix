# Status

Phase 1 is complete: Tasks 1-12 and the final review's fix list are done. Phase 2 Tasks 1 (the review residuals N1-N7, N9, N10, O5), 2 (fedimint 0.13 in the build, devimint smoke test), 3 (store, seed, data dir lock), 4 (additive API: paged history, operations, recovery words, shutdown) and 5 (`FederationPort`, `FedimintPort` over both module generations, scriptable fake) are done. The whole UI runs over `DemoBackend`. Nothing calls the port yet, and the app only starts with `--demo`.

## What works (commands run in `nix develop`)
- `just clippy`, `just test` (90 core unit, 4 demo-flow, 17 operation, 78 gtk unit tests).
- `nix build` (fails if the binary cannot load its libraries; `result/bin/fedimix --demo` was started once under headless sway with a clean environment), or `just build-nix` for that plus `nix flake check` (clippy, fmt, tests on the same source).
- `just test-integration` (slow; devimint with both module sets, 8 tests each: join and reopen, deposit, Lightning receive and pay, withdraw, minimum deposit, bad invites, the smoke test). Uses `nix develop .#integration --accept-flake-config`; plain `nix develop` does not touch fedimint.
- `just demo` / `just run`; `--demo-scenario funded|below-target|no-saving|first-run|offline|exit-deferred|long-names|device-disconnected|preview-expiry|device-rejects|send-stuck|long-history`, `--demo-page ...`.
- Core API is the Phase 2 shape: operations with state events and cancel, expiring previews, exit quotes, `parse_destination`, device identity, queue deferrals.

## Verified headlessly (sway + grim, GTK_A11Y=none, cairo renderer)
- `just interact-demo`: 22 scripted flows (21: history "Show more" on both tabs at 360 px; 22: recovery words) through `--demo-drive`, with assertions on outcomes. Removing the Refill guard, the Send lock, the single window or the grace-period refusal made its flow fail.
- `just screenshot-all`: every listed page at 480x640 and 360x640 with no CRITICAL or panic. `long-names` and `device-disconnected` were read by eye at 360 px; nothing overflows.

## Not verified
- Failed `set_active_federation` reverting the radio: the demo has one federation, so nothing can fail. No flow reaches it.
- The loading spinner and the error page with Retry for a failed first snapshot: the demo never fails to load, so no flow reaches them.
- A panic in a core future: covered by a unit test of `CoreBridge::call`, not through the UI.
- Real pointer/keyboard input (the driver emits clicks and sets text), Escape as a key, the Apply button's own click (the driver emits `apply`), the header menu popover (opened through actions), Enter in entries.
- Dark theme, accessibility, a second process launch (flows activate in-process), a real desktop (sway only), hardware, any real backend.
- `parse_destination` is lexical (base58 by shape); `validate_address` checks checksum and network but nothing calls it until the real backend.
- Window close and Quit wait for `shutdown` (5 s cap): unit-tested cap, exercised by every flow ending through the quit action; a hung backend is not run in the UI.
- Startup `operations()` watchers: the demo never has a running operation at start, so only the unit test of `resumed_outcome` covers them.

## Next
Phase 2: real Fedimint backend. A build and devimint spike is running; the plan follows from it. The plan owns bootstrap (open/shutdown/seed), paged history and the network setting.
Phase 2 Task 6 onward. Persistence (`store.rs`, `seed.rs`, `CoreConfig`/`Network`) and `federation/` exist but nothing calls them until the real backend. Still open from the re-review: O1-O4 (not in Task 1) and N8 (accepted).

## Needs user
Nothing.
