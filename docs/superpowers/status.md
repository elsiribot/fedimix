# Status

Phase 2 (real Spending wallet) is complete: Tasks 1-10 and the final review's fixes. `fedimix` runs on the real backend (fedimint 0.13, both module generations): Join with an invite code, Lightning and on-chain receive and send, restart, recovery of a lost client database. Saving, exits and cover traffic are Phase 3 and later; the real UI hides their controls.

## What works (commands run in `nix develop`)
- `just run [--data-dir PATH] [--network bitcoin|signet|regtest]`: the real wallet. `just demo`: the complete UI over the demo backend.
- `just clippy`, `just fmt`, `just test` (includes `tests/real_backend.rs` over a fake port).
- `just test-integration`: devimint, both module sets, 34 tests each at `FM_FED_SIZE=1` (default); `FM_FED_SIZE=4` last passed with 24, before the final fixes.
- `just test-real-ui`: the real binary through Join, Lightning receive and send, restart, lock, wrong network, under headless sway.
- `just interact-demo`, `just screenshot-all`: scripted demo flows and screenshots at 480 and 360 px.
- `nix build` / `just build-nix`: wrapped release binary plus flake checks. Release build from clean: 7 min 16 s, 88 MiB, 64 MiB stripped.

## Not verified
- Mainnet and signet: every real run was regtest.
- A real on-chain receive or send through the UI (covered below the UI by `devimint.rs`); a real recovery on screen.
- Real pointer and keyboard input, dark theme, accessibility, a real desktop (sway only), hardware; the clipboard against another Wayland client or a clipboard manager.
- Guardians gone for good: `a_stopped_federation_leaves_the_wallet_readable` stops them with SIGSTOP and brings them back.
- A hung backend at shutdown (30 s cap) in the UI; `NON_UNIQUE` launcher behavior on a desktop.
- That no Electrum traffic follows a proxy change beyond the one request in flight (at most 10 scripts, its answer read up to 30 s; `decisions.md`, Task 5 fix I1): tested against a stub server, not through a proxy. A Jade PIN step already under way cannot be stopped.
- A send against real hardware or a public server; a dropped send is exercised with fakes only. Details: `decisions.md`.

## Next
Phase 3 (Saving, hardware wallet), Tasks 1-6 done: chain access, the watch-only wallet, devices (`HwiPort`, compile-only; `SoftSigner` under `testing`), `RealBackend` Saving state and sync, and Task 6: `prepare_send`/`confirm_send`/`cancel_operation` for Saving, with change to a federation deposit address, small change kept, and the signed transaction stored before broadcast (`saving_signed`, schema 6), rebroadcast until settled. The core sends; GTK's Send and Refill arrive in Task 8. Next: Task 7 (refill). Open from the Phase 2 re-review: O1-O4 and N8 (accepted).

## Needs user
- A real-network trial with small amounts before trusting the wallet with more.
- Decide restore-from-words in the app: today the words alone do nothing there. The README has the manual path (new folder, words in `seed` mode 0600, join with invite codes). Settings now shows each invite with Copy.
- SOCKS5 for federation traffic: fedimint 0.13 has no upstream support, so a set proxy blocks federation connections (a sentence says so).
- The seed sits in a plain 0600 file in the data folder; decide whether that is enough or it needs encryption.
- Hardware wallet testing needs a device and you. Untested by construction: Ledger signing (`Ledger::with_wallet` with the default `wpkh(@0/**)` policy, `hmac = None`: whether the Ledger app accepts it), BitBox02 pairing and PSBT pruning, Coldcard, Jade, and the sentences for "open the Bitcoin app" and a locked Ledger (matched from the client's status text). Jade's PIN step talks to Blockstream's PIN server outside any proxy.
