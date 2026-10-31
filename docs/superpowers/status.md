# Status

Phase 1 is complete: Tasks 1-12 and the final review's fix list are done. Phase 2 Tasks 1 (the review residuals N1-N7, N9, N10, O5) and 2 (fedimint 0.13 in the build, devimint smoke test) are done. The whole UI runs over `DemoBackend`. There is no real backend yet, and the app only starts with `--demo`.

## What works (commands run in `nix develop`)
- `just clippy`, `just test` (45 core unit, 4 demo-flow, 17 operation, 69 gtk unit tests).
- `nix build` (fails if the binary cannot load its libraries; `result/bin/fedimix --demo` was started once under headless sway with a clean environment), or `just build-nix` for that plus `nix flake check` (clippy, fmt, tests on the same source).
- `just test-integration` (slow; devimint with both module sets, each runs `smoke_join_and_read_modules`: join a federation with RocksDB, balance 0). Uses `nix develop .#integration --accept-flake-config`; plain `nix develop` does not touch fedimint.
- `just demo` / `just run`; `--demo-scenario funded|below-target|no-saving|first-run|offline|exit-deferred|long-names|device-disconnected|preview-expiry|device-rejects|send-stuck`, `--demo-page ...`.
- Core API is the Phase 2 shape: operations with state events and cancel, expiring previews, exit quotes, `parse_destination`, device identity, queue deferrals.

## Verified headlessly (sway + grim, GTK_A11Y=none, cairo renderer)
- `just interact-demo`: 20 scripted flows through `--demo-drive`, with assertions on outcomes. Removing the Refill guard, the Send lock, the single window or the grace-period refusal made its flow fail.
- `just screenshot-all`: every listed page at 480x640 and 360x640 with no CRITICAL or panic. `long-names` and `device-disconnected` were read by eye at 360 px; nothing overflows.

## Not verified
- Failed `set_active_federation` reverting the radio: the demo has one federation, so nothing can fail. No flow reaches it.
- The loading spinner and the error page with Retry for a failed first snapshot: the demo never fails to load, so no flow reaches them.
- A panic in a core future: covered by a unit test of `CoreBridge::call`, not through the UI.
- Real pointer/keyboard input (the driver emits clicks and sets text), Escape as a key, the Apply button's own click (the driver emits `apply`), the header menu popover (opened through actions), Enter in entries.
- Dark theme, accessibility, a second process launch (flows activate in-process), a real desktop (sway only), hardware, any real backend.
- `parse_destination` does not verify checksums and has no base58 addresses; real validation is Phase 2.

## Next
Phase 2: real Fedimint backend. A build and devimint spike is running; the plan follows from it. The plan owns bootstrap (open/shutdown/seed), paged history and the network setting.
Phase 2 Task 3 onward. Still open from the re-review: O1-O4 (not in Task 1) and N8 (accepted).

## Needs user
Nothing.
