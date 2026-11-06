# Status

Phase 3 (Saving) is complete; Phase 4 Tasks 1-9 are done. `fedimix` runs Spending and Saving on the real backend: Saving on a hardware wallet (Ledger, Coldcard, Jade, BitBox02) or "This computer", with Receive, Send, Refill and history. Money above Spending's target queues in denominations and exits to Saving on its own or early by hand, labelled with its anonymity estimate. Cover traffic (Phase 5) stays hidden.

## What works (commands run in `nix develop`)
- `just run [--data-dir PATH] [--network bitcoin|signet|regtest]`: the real wallet. `just demo`: the complete UI over the demo backend.
- `just clippy`, `just fmt`, `just test`.
- `just test-integration`: devimint, both module sets (40 tests each at `FM_FED_SIZE=1`).
- `just test-real-ui`: the real binary through Join, Lightning, restart, Saving on "This computer" (setup, receive, send, refill) a `--test-signer` Ledger stand-in, and exits (`--exit-timing fast`: an early exit and an automatic one, both landing labelled), under headless sway.
- `just interact-demo`, `just screenshot-all`, `nix build` / `just build-nix`.

## Not verified
- Mainnet and signet: every real run was regtest. No real hardware device has signed anything.
- The GTK half of 6c91584 (Saving's scanning state, `open_again`, Receive reload) had no reviewer: only its gates (clippy, tests, real-UI runs) checked it.
- Real pointer input, dark theme, accessibility, a real desktop (sway only).

## Next
Phase 4 (scheduler and anonymity estimate): plan `docs/superpowers/plans/2026-10-30-fedimix-phase-4.md`. Task 1 spike: sessions decode on both generations. Tasks 2, 3 and 8 done (Task 8: the demo backend exits on its own, `just demo` scenario `exits`, flow 30). Task 2: pure `scheduler` and `anonymity` modules with unit tests; Task 4 calls `anonymity`. Task 3 done: schema 10 stores the exit queue, exit addresses and session deposits; Saving coins at an exit address are tagged by it and read "From Spending"; Lightning receives keep their session. Task 4 done: each connection syncs the federation's signed sessions (v1 and v2 deposits, LNv2 receives) into the store from the join on, retrying a session that fails; `FederationInfo.anonymity_estimate` counts other people's deposits of at least 0.001 BTC in the last 480 synced sessions, and `Core::item_estimate` an item's. Each outpoint counts once, and our own Lightning receives are left out from the moment the invoice is made (fix round 1). Task 5 done: the real backend books the money above the target in denominations (`queue_state`, Spending's moving and for spending); a Spending send's review says what it takes from the queue and its confirm shrinks it newest first, as a higher target does; Saving's `in_flight` counts exits on their way; tending the queue never delays a balance event, and a send cut off before its fit still takes from the queue after a restart. Task 6 done: `prepare_exit` and `exit_now` withdraw a queued denomination to a fresh Saving address (early exit: the fee is shown, the fee guard does not apply, the grace period does). The exit books its money before it is written, resumes or requeues after a crash, never reuses an address, and lands labelled `EarlyExit` with its source federation once its coin confirms (`Notice::ExitLanded`). Task 6 review fix: a withdrawal whose outcome cannot be told (`WatchUpdate::Unknown`) never requeues its exit; it stays `Withdrawing` until its coin lands. Task 7 done: one scheduler task draws each ready item's exit time (`Scheduled{at}`, redrawn on start, reconnect, settings change and a new estimate) and starts due exits through the same `start_exit`, labelled `Anonymity{estimate}`, at most one per federation per tick and 1 h apart; nothing leaves offline, in the grace period, above the fee guard or when Spending cannot pay the fee, and queued items say which (`Deferred{..}`). A `testing` build takes `--exit-timing fast` (the core refuses it on bitcoin). Fix round 1: an exit whose outcome is unknown still spaces the next by 1 h, its history row reads "outcome unknown" (Complete once its coin lands), it is not counted as depositing, and a failing start backs off from 60 s to 1 h. Task 9 done: the real app shows all of it: Settings' Exit group and each federation's "anon ~N" ("Most private" on the largest of several), Spending's "moving to Saving ›" link, the Moving to Saving page with Exit now (its warning names the estimate), and Saving's "+X depositing ›". An exit whose outcome is unknown reads `OutcomeUnknown`: its row says "outcome unknown" and the page says what happens either way. Next: Tasks 10-11.
Demo fixes done: Saving's refill (or send) row turns final when Spending credits its deposit, no restart needed; the header shows "Regtest" or "Signet" off mainnet.

## Needs user
- Hardware testing on each device kind: Ledger (policy `wpkh(@0/**)`, no hmac), BitBox02 pairing, Coldcard, Jade (its PIN step uses Blockstream's server outside any proxy).
- Trezor: not supported (async-hwi has none); decide whether to add it.
- Restore-from-words in the app: today the words alone do nothing there; the README has the manual path.
- A real-network trial with small amounts (signet, then bitcoin).
- Whether to upstream `next_receive_address` (and address display on devices) to async-hwi or BDK.
- Seed encryption: the seed is a plain 0600 file, and with "This computer" it holds Saving's key too. No key is zeroized in memory.
- A federation deposit that is never credited blocks every refill for good: no in-app way out yet.
- SOCKS5 for federation traffic: fedimint 0.13 has none.
- An exit whose outcome is unknown has no user action to settle it, and its history row reads "failed".
