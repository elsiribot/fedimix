# Decisions

- Commits are made as repo-local identity `elsiribot` because no git identity was configured.
- `scripts/screenshot.sh` builds the binary before starting sway, so the 15 s window-map timeout excludes compilation.
- `just screenshot` writes to `target/screenshots/<name>.png`, which is already gitignored via `/target/`.
- Screenshot harness waits for sway app_id `fedimix` (what GTK reports under headless sway), not the reverse-DNS application id.
- `Amount::from_sat` uses plain multiplication (no saturation); max supply (2.1e15 sat) fits u64 msat, so overflow cannot occur for valid values.
- `EventStream` derives Clone (async_channel receivers are cloneable, work-sharing) so the UI can hand it to tasks easily.
- tokio is a dev-dependency only (features macros, rt) in core; the async API stays runtime-agnostic.
- DemoBackend keeps event senders in a separate std Mutex (not the state Mutex) because `subscribe()` is sync and must also be reachable from the delayed-notification task; each subscriber gets its own unbounded channel and closed ones are pruned on emit.
- `SendConfirmed` is emitted via `tokio::spawn` + sleep only when a tokio runtime is current; without one it is skipped rather than panicking.
- tokio is now a regular dependency of core (features sync, time) for the demo backend's Mutex and sleep; the Backend trait stays runtime-agnostic.
- Demo refill checks the signing device before the target gap, so NoSaving reports NotSetUp rather than "at target"; `exit_now` also requires a device.
- Task 4: `queue_row` takes the anonymity target as a parameter for the meter; caption fraction is for_spending/target below target, else for_spending/(for_spending+moving); `format::date` computes the local UTC offset via glib and delegates to a pure `date_with_offset` so tests are timezone independent.
