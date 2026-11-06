# Status

Phase 3 (Saving) is complete. `fedimix` runs Spending and Saving on the real backend: Saving on a hardware wallet (Ledger, Coldcard, Jade, BitBox02) or "This computer", with Receive, Send, Refill and history. Exits and cover traffic (Phase 4) stay hidden.

## What works (commands run in `nix develop`)
- `just run [--data-dir PATH] [--network bitcoin|signet|regtest]`: the real wallet. `just demo`: the complete UI over the demo backend.
- `just clippy`, `just fmt`, `just test`.
- `just test-integration`: devimint, both module sets (38 tests each at `FM_FED_SIZE=1`).
- `just test-real-ui`: the real binary through Join, Lightning, restart, Saving on "This computer" (setup, receive, send, refill) and a `--test-signer` Ledger stand-in, under headless sway.
- `just interact-demo`, `just screenshot-all`, `nix build` / `just build-nix`.

## Not verified
- Mainnet and signet: every real run was regtest. No real hardware device has signed anything.
- The GTK half of 6c91584 (Saving's scanning state, `open_again`, Receive reload) had no reviewer: only its gates (clippy, tests, real-UI runs) checked it.
- Real pointer input, dark theme, accessibility, a real desktop (sway only).

## Next
Phase 4 (scheduler and anonymity estimate): plan `docs/superpowers/plans/2026-10-30-fedimix-phase-4.md`. Task 1 spike: sessions decode on both generations. Tasks 2, 3 and 8 done (Task 8: the demo backend exits on its own, `just demo` scenario `exits`, flow 30). Task 2: pure `scheduler` and `anonymity` modules with unit tests; Task 4 calls `anonymity`. Task 3 done: schema 10 stores the exit queue, exit addresses and session deposits; Saving coins at an exit address are tagged by it and read "From Spending"; Lightning receives keep their session. Task 4 done: each connection syncs the federation's signed sessions (v1 and v2 deposits, LNv2 receives) into the store from the join on, retrying a session that fails; `FederationInfo.anonymity_estimate` counts other people's deposits of at least 0.001 BTC in the last 480 synced sessions, and `Core::item_estimate` an item's. Each outpoint counts once, and our own Lightning receives are left out from the moment the invoice is made (fix round 1). Task 5 done: the real backend books the money above the target in denominations (`queue_state`, Spending's moving and for spending); a Spending send's review says what it takes from the queue and its confirm shrinks it newest first, as a higher target does; Saving's `in_flight` counts exits on their way; tending the queue never delays a balance event, and a send cut off before its fit still takes from the queue after a restart. Items read `Queued` until Task 7; the GTK app still hides all of it (Task 9). Next: Tasks 6, 7, 9-11.
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
