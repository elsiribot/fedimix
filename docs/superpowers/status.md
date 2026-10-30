# Status

Phase 1, Tasks 1-2 done (workspace split/dev shell/screenshot harness; core API types, events, Backend trait, Fedimix handle).

## What works
- `nix develop -c just build|test|clippy|fmt` on the `fedimix-core` + `fedimix-gtk` workspace.
- `nix develop -c just run` launches the placeholder window.
- `nix develop -c just screenshot <name> [args...]` renders the app headlessly
  (sway + grim) to `target/screenshots/<name>.png`.

- `fedimix-core` exposes `Amount`, state/preview types, `Event`/`EventStream`, `FedimixError`, `Backend` (async_trait) and the `Fedimix` forwarding handle; 4 unit tests via `nix develop -c just test`.

## Next
Task 3.

## Needs user
Nothing.
