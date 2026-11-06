# Status

Phases 3 (Saving) and 4 (automatic private exits) are complete. `fedimix` runs Spending and Saving on the real backend: Saving on a hardware wallet (Ledger, Coldcard, Jade, BitBox02) or "This computer", with Receive, Send, Refill and history. Money above Spending's target queues in denominations and exits to Saving on its own or early by hand, labelled with its anonymity estimate. Cover traffic (Phase 5) stays hidden.

## What works (commands run in `nix develop`)
- `just run [--data-dir PATH] [--network bitcoin|signet|regtest]`: the real wallet. `just demo`: the complete UI over the demo backend.
- `just clippy`, `just fmt`, `just test`.
- `just test-integration`: devimint, both module sets (40 tests each at `FM_FED_SIZE=1`).
- `just record-exits`: records exits on the real backend (regtest devimint, `--exit-timing fast`) to `~/fedimix-demo/phase4/real-exits/exits.mp4`; `just record-exits-demo` records the simulated ones. `just test-real-ui` runs `scripts/real-flows/exits.drive`; `README.md` explains the rules.
- `just test-real-ui`: the real binary through Join, Lightning, restart, Saving on "This computer" (setup, receive, send, refill) a `--test-signer` Ledger stand-in, and exits (`--exit-timing fast`: an early exit and an automatic one, both landing labelled), under headless sway.
- `just interact-demo`, `just screenshot-all`, `nix build` / `just build-nix`.

## Not verified
- Mainnet session sizes and sync time, LNv1 federations, exit timing under real fees.
- The local devimint has no other depositors: the anon estimate reads 0 in every real run.
- Mainnet and signet: every real run was regtest. No real hardware device has signed anything.
- The GTK half of 6c91584 (Saving's scanning state, `open_again`, Receive reload) had no reviewer: only its gates (clippy, tests, real-UI runs) checked it.
- Real pointer input, dark theme, accessibility, a real desktop (sway only).

Final review fixes: exit fee quotes go to a fixed stand-in address, never a Saving one (v1 shows quote addresses to every guardian); the queue no longer oscillates while a send has no operation; the attainable estimate never projects past the sessions its rate was measured on.

Next: Phase 5 (cover traffic).

## Needs user
- Hardware testing on each device kind: Ledger (policy `wpkh(@0/**)`, no hmac), BitBox02 pairing, Coldcard, Jade (its PIN step uses Blockstream's server outside any proxy).
- Trezor: not supported (async-hwi has none); decide whether to add it.
- Restore-from-words in the app: today the words alone do nothing there; the README has the manual path.
- A real-network trial with small amounts (signet, then bitcoin), exits included.
- Whether to upstream `next_receive_address` (and address display on devices) to async-hwi or BDK.
- Seed encryption: the seed is a plain 0600 file, and with "This computer" it holds Saving's key too. No key is zeroized in memory.
- A federation deposit that is never credited blocks every refill for good: no in-app way out yet.
- SOCKS5 for federation traffic: fedimint 0.13 has none.
- An unknown exit outcome cannot be cleared from the app yet (its row reads "outcome unknown").
- The anon estimate has not been seen above 0 in a real run; the outcome mapping in onchain.rs is untested.
- v1 fee quotes use one fixed stand-in address, so guardians can tell a quote is from Fedimix (not whose); a random throwaway address per quote would hide that.
- The wait formula and spacing values; whether the active federation should follow the largest estimate; whether to count LNv1 receives.
