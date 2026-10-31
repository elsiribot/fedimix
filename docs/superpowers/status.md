# Status

Phase 2 (real Spending wallet) is complete: Tasks 1-10. `fedimix` runs on the real backend (fedimint 0.13, both module generations): Join with an invite code, Lightning and on-chain receive and send, restart, recovery of a lost client database. Saving, exits and cover traffic are Phase 3 and later; the real UI hides their controls.

## What works (commands run in `nix develop`)
- `just run [--data-dir PATH] [--network bitcoin|signet|regtest]`: the real wallet. `just demo`: the complete UI over the demo backend.
- `just clippy`, `just fmt`, `just test` (includes `tests/real_backend.rs` over a fake port).
- `just test-integration`: devimint, both module sets, 24 tests each, passed at `FM_FED_SIZE=1` (default) and `FM_FED_SIZE=4`.
- `just test-real-ui`: the real binary through Join, Lightning receive and send, restart, lock, wrong network, under headless sway.
- `just interact-demo`, `just screenshot-all`: scripted demo flows and screenshots at 480 and 360 px.
- `nix build` / `just build-nix`: wrapped release binary plus flake checks. Release build from clean: 7 min 16 s, 88 MiB, 64 MiB stripped.

## Not verified
- Mainnet and signet: every real run was regtest.
- A real on-chain receive or send through the UI (covered below the UI by `devimint.rs`); a real recovery on screen.
- Real pointer and keyboard input, dark theme, accessibility, a real desktop (sway only), hardware.
- The clipboard against another Wayland client or a clipboard manager.
- A hung backend at shutdown (30 s cap) in the UI; `NON_UNIQUE` launcher behavior on a desktop.
- Details: `decisions.md`.

## Next
Phase 3 (Saving, hardware wallet). Open from the re-review: O1-O4 and N8 (accepted).

## Needs user
- A real-network trial with small amounts before trusting the wallet with more.
- Decide restore-from-words on a new device: the words are shown, but no flow restores a wallet from them.
- SOCKS5 for federation traffic: fedimint 0.13 has no upstream support, so a set proxy blocks federation connections (a sentence says so).
- The seed sits in a plain 0600 file in the data folder; decide whether that is enough or it needs encryption.
- Hardware wallet testing for Phase 3 needs a device and you.
