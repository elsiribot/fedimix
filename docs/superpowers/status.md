# Status

Phase 1 is complete: Tasks 1-12 and the final review's fix list are done. The whole UI runs over `DemoBackend`. There is no real backend yet, and the app only starts with `--demo`.

## What works (commands run in `nix develop`)
- `just clippy`, `just test` (45 core unit, 4 demo-flow, 16 operation, 67 gtk unit tests).
- `nix build` (fails if the binary cannot load its libraries; `result/bin/fedimix --demo` was started once under headless sway with a clean environment), or `just build-nix` for that plus `nix flake check` (clippy, fmt, tests on the same source).
- `just demo` / `just run`; `--demo-scenario funded|below-target|no-saving|first-run|offline|exit-deferred|long-names|device-disconnected|preview-expiry|device-rejects`, `--demo-page ...`.
- Core API is the Phase 2 shape: operations with state events and cancel, expiring previews, exit quotes, `parse_destination`, device identity, queue deferrals.

## Verified headlessly (sway + grim, GTK_A11Y=none, cairo renderer)
- `just interact-demo`: 18 scripted flows through `--demo-drive`, with assertions on outcomes. Removing the Refill guard, the Send lock or the single window made its flow fail.
- `just screenshot-all`: every listed page at 480x640 and 360x640 with no CRITICAL or panic. `long-names` and `device-disconnected` were read by eye at 360 px; nothing overflows.

## Not verified
- Failed `set_active_federation` reverting the radio: the demo has one federation, so nothing can fail. No flow reaches it.
- The loading spinner before the first snapshot: the demo answers too fast for a flow to see it.
- Real pointer/keyboard input (the driver emits clicks and sets text), Escape as a key, the Apply button's own click (the driver emits `apply`), the header menu popover (opened through actions), Enter in entries.
- Dark theme, accessibility, a second process launch (flows activate in-process), a real desktop (sway only), hardware, any real backend.
- `parse_destination` does not verify checksums and has no base58 addresses; real validation is Phase 2.

## Next
Phase 2: real Fedimint backend. Its plan owns bootstrap (open/shutdown/seed), paged history and the network setting; the FFI crate comes later.

## Needs user
Nothing.
