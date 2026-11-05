# Status

Phase 2 (real Spending wallet) is complete: Tasks 1-10 and the final review's fixes. `fedimix` runs on the real backend (fedimint 0.13, both module generations): Join with an invite code, Lightning and on-chain receive and send, restart, recovery of a lost client database. Saving, exits and cover traffic are Phase 3 and later; the real UI hides their controls.

## What works (commands run in `nix develop`)
- `just run [--data-dir PATH] [--network bitcoin|signet|regtest]`: the real wallet. `just demo`: the complete UI over the demo backend.
- `just clippy`, `just fmt`, `just test` (includes `tests/real_backend.rs` over a fake port).
- `just test-integration`: devimint, both module sets, 36 tests each at `FM_FED_SIZE=1` (default); `FM_FED_SIZE=4` last passed with 24, before the final fixes.
- `just test-real-ui`: the real binary through Join, Lightning receive and send, restart, lock, Saving on "This computer" (setup, receive from bitcoind, send whose change reaches Spending, refill), the `--test-signer` Ledger stand-in declining a signature, wrong network, under headless sway. Built with `fedimix-gtk/testing`.
- `just interact-demo`, `just screenshot-all`: scripted demo flows and screenshots at 480 and 360 px.
- `nix build` / `just build-nix`: wrapped release binary plus flake checks. Release build from clean: 7 min 16 s, 88 MiB, 64 MiB stripped.

## Not verified
- Mainnet and signet: every real run was regtest.
- A Spending on-chain receive or send through the UI (covered below the UI by `devimint.rs`); a real recovery on screen. Saving's on-chain path is covered on regtest only.
- Real pointer and keyboard input, dark theme, accessibility, a real desktop (sway only), hardware; Task 8 screens read at 800 px (real) and 480 px (demo, flows 26-29), not 360 px; the clipboard against another Wayland client or a clipboard manager.
- Guardians gone for good: `a_stopped_federation_leaves_the_wallet_readable` stops them with SIGSTOP and brings them back.
- A hung backend at shutdown (30 s cap) in the UI; `NON_UNIQUE` launcher behavior on a desktop.
- That no Electrum traffic follows a proxy change beyond the one request in flight (at most 10 scripts, its answer read up to 30 s; `decisions.md`, Task 5 fix I1): tested against a stub server, not through a proxy. A Jade PIN step already under way cannot be stopped.
- A send against real hardware or a public server; a dropped send is exercised with fakes only. Details: `decisions.md`.

## Next
Phase 3 Tasks 1-8 done (details in `decisions.md`). Task 8 put Saving on screen in the real app: setup with hardware wallets then "This computer", the first scan and an unopened wallet shown as such (no stand-in balance; Receive waits for the scan), Receive, Send with change and notes, Refill ("Refill sent", failures and refusals in alerts), stalled/lost/contested history rows; Exit and Cover stay hidden, as does the `in_flight` line. Next: Task 9 (close out). Open from the Phase 2 re-review: O1-O4 and N8 (accepted).

## Needs user
- A real-network trial with small amounts before trusting the wallet with more.
- Decide restore-from-words in the app: today the words alone do nothing there. The README has the manual path (new folder, words in `seed` mode 0600, join with invite codes). Settings now shows each invite with Copy.
- SOCKS5 for federation traffic: fedimint 0.13 has no upstream support, so a set proxy blocks federation connections (a sentence says so).
- The seed sits in a plain 0600 file in the data folder; decide whether that is enough or it needs encryption. With the software Saving wallet it holds Saving's key too; no key or seed is zeroized in memory. Saving cannot be switched to a hardware wallet once set up.
- Hardware wallet testing needs a device and you. Untested by construction: Ledger signing (`Ledger::with_wallet` with the default `wpkh(@0/**)` policy, `hmac = None`: whether the Ledger app accepts it), BitBox02 pairing and PSBT pruning, Coldcard, Jade, and the sentences for "open the Bitcoin app" and a locked Ledger (matched from the client's status text). Jade's PIN step talks to Blockstream's PIN server outside any proxy.
