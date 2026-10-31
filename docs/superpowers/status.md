# Status

Phase 2 (real Spending wallet) is complete: Tasks 1-10 and the final review's fixes. `fedimix` runs on the real backend (fedimint 0.13, both module generations): Join with an invite code, Lightning and on-chain receive and send, restart, recovery of a lost client database. Saving, exits and cover traffic are Phase 3 and later; the real UI hides their controls.

## What works (commands run in `nix develop`)
- `just run [--data-dir PATH] [--network bitcoin|signet|regtest]`: the real wallet. `just demo`: the complete UI over the demo backend.
- `just clippy`, `just fmt`, `just test` (includes `tests/real_backend.rs` over a fake port).
- `just test-integration`: devimint, both module sets, 26 tests each at `FM_FED_SIZE=1` (default); `FM_FED_SIZE=4` last passed with 24, before the final fixes.
- `just test-real-ui`: the real binary through Join, Lightning receive and send, restart, lock, wrong network, under headless sway.
- `just interact-demo`, `just screenshot-all`: scripted demo flows and screenshots at 480 and 360 px.
- `nix build` / `just build-nix`: wrapped release binary plus flake checks. Release build from clean: 7 min 16 s, 88 MiB, 64 MiB stripped.

## Not verified
- Mainnet and signet: every real run was regtest.
- A real on-chain receive or send through the UI (covered below the UI by `devimint.rs`); a real recovery on screen.
- Real pointer and keyboard input, dark theme, accessibility, a real desktop (sway only), hardware.
- The clipboard against another Wayland client or a clipboard manager.
- Guardians gone for good: `a_stopped_federation_leaves_the_wallet_readable` stops them with SIGSTOP and brings them back.
- A hung backend at shutdown (30 s cap) in the UI; `NON_UNIQUE` launcher behavior on a desktop.
- Details: `decisions.md`.

## Next
Phase 3 (Saving, hardware wallet), Task 1 done: the Saving dependencies (bdk_wallet, bdk_esplora, bdk_electrum, async-hwi, rusqlite 0.31; udev and hidapi in the nix build), the per-network chain server default (M14), N1-N3, and Settings shows each federation's invite with Copy. Next: Task 2 of the Phase 3 plan. Open from the Phase 2 re-review: O1-O4 and N8 (accepted). Flow 03 now runs each operation state for 3 s (`# step-ms:` header, `--demo-step-ms`); the flake was never reproduced here, so the cause (a shot outlasting a 300 ms state) is from reading.

## Needs user
- A real-network trial with small amounts before trusting the wallet with more.
- Decide restore-from-words in the app: today the words alone do nothing there. The README has the manual path (new folder, words in `seed` mode 0600, join with invite codes). Settings now shows each invite with Copy.
- v2 reuses its deposit address until a payment to it is seen, and Receive says so. Phase 3's fresh change address needs either an upstream `next_receive_address` or a refusal of concurrent sends: decide before the Phase 3 design is final.
- SOCKS5 for federation traffic: fedimint 0.13 has no upstream support, so a set proxy blocks federation connections (a sentence says so).
- The seed sits in a plain 0600 file in the data folder; decide whether that is enough or it needs encryption.
- Hardware wallet testing for Phase 3 needs a device and you.
