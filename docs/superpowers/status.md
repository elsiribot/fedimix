# Status

Phase 1, Tasks 1-3 done (workspace split/dev shell/screenshot harness; core API types, events, Backend trait, Fedimix handle; DemoBackend).

## What works
- `nix develop -c just build|test|clippy|fmt` on the `fedimix-core` + `fedimix-gtk` workspace.
- `nix develop -c just run` launches the placeholder window.
- `nix develop -c just screenshot <name> [args...]` renders the app headlessly
  (sway + grim) to `target/screenshots/<name>.png`.

- `fedimix-core` exposes `Amount`, state/preview types, `Event`/`EventStream`, `FedimixError`, `Backend` (async_trait) and the `Fedimix` forwarding handle; `DemoBackend::new(DemoScenario)` (funded, below-target, no-saving, first-run, offline) implements Backend with mockup data; 21 unit tests via `nix develop -c just test`.

## Next
Task 4.

## Needs user
Nothing.
