# Fedimix Phase 3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Saving real: import an xpub from a hardware wallet, sync a watch-only BDK wallet, send with change into the federation, and refill Spending from the smallest coins.

**Architecture:** `fedimix-core` gains a `saving` module around `bdk_wallet`, with two seams like Phase 2's `FederationPort`: `ChainClient` (Esplora or Electrum) and `Signer` (async-hwi or, in tests only, a software signer). `RealBackend` keeps owning state, operations and events; its Saving side reuses the store's history, watches and operation reporting.

**Tech Stack:** bdk_wallet 3.2.0 (`rusqlite` feature), bdk_esplora 0.22.3, bdk_electrum 0.24.1, async-hwi 0.0.35, rusqlite 0.31.

**Spec:** `docs/superpowers/specs/2026-10-30-fedimix-design.md`

**Reference (gitignored, working code):** `.superpowers/sdd/saving-spike.md`, spike source `/tmp/claude-1000/-home-user-projects-fedimix/00c8fe2b-d6cb-4b31-95aa-9c55dbd4d7d3/scratchpad/saving-spike/src/{signer.rs,bin/bdk.rs,bin/fm.rs}`.

## Global Constraints

- Stay on `master`; no branches or worktrees. Never `git worktree prune` or `git gc`. Never commit `ANALYSIS.md` or `.superpowers/`.
- Before every commit: `nix develop -c just clippy` and `nix develop -c just test` pass. Commit subjects are conventional.
- Core public API stays owned and FFI-friendly. No bdk, bitcoin or async-hwi type crosses it. Nothing under `crates/fedimix-gtk` imports them.
- "Only the hardware wallet signs; the app never holds cold keys." The software signer exists only under the `testing` feature and never in a release binary. Amended 2026-11-05 at the user's request: Saving may also be a software wallet on this computer, keyed from the recovery words (Task 7b). The `testing` `SoftSigner` and its hooks still never reach a release binary.
- Each Saving send's change output is a fresh federation deposit address. Below the federation's minimum deposit it stays in Saving, labelled "small change".
- Refill: smallest-first, no change output, never automatic, signed on the device.
- Never charge more than the preview shows.
- `just test-integration` and `just test-real-ui` stay green; demo stays working.
- Update `docs/superpowers/status.md` (at most ~30 lines) after each task; undecided design goes in `docs/superpowers/decisions.md`.

## Decisions fixed by this plan

Record each in `decisions.md` in the task that implements it.

- **rusqlite goes to 0.31.** bdk's `rusqlite` feature needs it; two `libsqlite3-sys` cannot link. BDK tables live in `fedimix.db` through a second connection; Fedimix's own tables are unchanged.
- **No Trezor.** Spec section 3 lists it; async-hwi 0.0.35 has none, and spec prefers async-hwi. `DeviceKind::Trezor` stays in the API, never returned. Listed under Needs user.
- **BIP84 single-sig:** `wpkh([fp/84h/<coin>h/0h]xpub/{0,1}/*)`, coin 0 on bitcoin, 1 elsewhere, account 0.
- **Plain BIP84 addresses cannot be shown on a device through async-hwi.** The Saving Receive dialog says so: "Your device cannot display this address." Unverified addresses are the cost; a Miniscript policy route is rejected for Phase 3 as untestable without hardware.
- **v2 change addresses (review I2).** fedimint's `receive()` repeats until the scanner advances, and its derivation is private. Fedimix records each federation address it puts in a PSBT. A later Saving send or refill that gets the same address from `deposit_address()` is refused: "Your last change is still reaching Spending. Try again in a few minutes." This keeps spec's "fresh" without patching fedimint. Addresses shown in Receive are not recorded, because an unpaid one would block Saving for ever; that reuse risk stays, stated in the dialog. v1 addresses are fresh.
- **Small change margin.** `min_deposit` moves on v2, so change goes to the federation only above `2 × min_deposit`.
- **One Saving operation at a time,** refused with a sentence: there is one device, and BDK's selection must see the previous spend.
- **Crash safety.** The signed transaction is stored before broadcast and rebroadcast on restart until seen. Rebroadcast of the same transaction cannot pay twice.
- **Chain scan.** First scan and after-restore `stop_gap = 100`; routine sync uses revealed scripts every 60 s, and 10 s while an operation or `in_flight` is active.
- **Fee rate.** Esplora/Electrum estimate for 6 blocks. None available: 2 sat/vB on regtest; elsewhere `prepare_*` returns a sentence and nothing is built.
- **`fee_rate_sat_vb` from `SendRequest` is honoured** when present; `prepare_refill` always estimates.
- **Default chain server per network** (review M14): bitcoin `https://blockstream.info/api`, signet `https://mempool.space/signet/api`, regtest none (must be set). A stored mainnet URL in a non-mainnet folder reads as unset.
- **Proxy.** `socks5h://`, so DNS resolves at the proxy. Chain traffic fails closed if the proxy is down.
- **Device polling.** `detect_devices` on demand; the backend also polls every 5 s while the Send or Refill review is open, or during AwaitingDevice, not otherwise.
- **Refill overshoot** simply raises Spending's balance; Phase 4's queue picks it up. Phase 3 only shows the number.

## Review Focus

1. Device unplugged or rejecting during signing: the operation ends cleanly, no broadcast, no stuck row, and the coins are usable again.
2. Crash between signing and broadcast, or after broadcast before the sync sees it: restart rebroadcasts or recognises the transaction; never a second spend.
3. A federation deposit address from one federation used while another is active, or the federation offline during Send: change must go to the active federation, and offline means a sentence before signing.
4. Frozen coins, unconfirmed coins and dust: refill and sends never pick frozen coins; refill never overshoots into `gap not covered` silently; sends above spendable report `InsufficientFunds`.
5. Reorg or a chain server that lies or is unreachable: balances show last known state with an offline state; a dropped unconfirmed tx does not stay Pending for ever.

---

### Task 1: Residuals and dependencies

**Files:** Modify `Cargo.toml`, `crates/fedimix-core/Cargo.toml`, `flake.nix`, `crates/fedimix-core/src/{store,seed}.rs`, `real/mod.rs`, `crates/fedimix-core/src/api.rs`, `crates/fedimix-gtk/src/ui/settings_page.rs`, `scripts/demo-flows/03-send-saving.drive`.

- [ ] **Step 1: Failing tests:** `missing_seed_with_a_surviving_federations_folder_is_refused` (the final re-review's gap), `default_chain_server_follows_network`, `mainnet_url_in_a_signet_folder_reads_as_unset`, `present::federation_row_offers_its_invite`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** Move rusqlite to 0.31; add bdk_wallet, bdk_esplora, bdk_electrum, and async-hwi with features `ledger,coldcard,bitbox,jade` (no `service`); udev, hidapi and pkg-config to flake inputs and the nix build. Settings shows each federation's invite code with Copy, since the README tells users to keep them (add `FederationInfo.invite`). Fix flow 03's flake: find the cause first; if the shot outlasts a demo state, hold the state through `DemoTiming`.
- [ ] **Step 4: Verify** `just test`, `just test-integration`, `just interact-demo` three times, `nix build`.
- [ ] **Step 5: Commit** `chore: add the Saving dependencies and settle the Phase 2 residuals`.

### Task 2: Chain access

**Files:** Create `crates/fedimix-core/src/saving/{mod,chain,fake_chain}.rs`. Extend `tests/devimint.rs`.

**Interfaces:**
- Produces (crate-private):
```rust
#[async_trait] trait ChainClient: Send + Sync {
  async fn full_scan(&self, request: FullScanRequest<KeychainKind>, stop_gap: usize) -> Result<FullScanResponse<KeychainKind>>;
  async fn sync(&self, request: SyncRequest<(KeychainKind, u32)>) -> Result<SyncResponse>;
  async fn broadcast(&self, tx: &Transaction) -> Result<()>;   // already-known is Ok
  async fn fee_rate_sat_vb(&self, target_blocks: u16) -> Result<Option<f64>>;
  async fn tx_seen(&self, txid: Txid) -> Result<bool>;
}
fn connect_chain(backend: &ChainBackend, proxy: Option<&str>) -> Result<Arc<dyn ChainClient>>;
```

- [ ] **Step 1: Failing devimint tests:** `esplora_and_electrum_see_the_same_balance`, `broadcasting_twice_is_ok`, `proxy_down_fails_closed` (a closed local port; assert no direct connection through a listener on the real port count of 0).
- [ ] **Step 2: Run; expect FAIL.** The script exports `FM_PORT_ESPLORA`, `FM_PORT_ESPLORA_ELECTRUM` through devimint.
- [ ] **Step 3: Implement** from the spike. Timeouts: 30 s per request. Errors are sentences; unreachable is `Offline`.
- [ ] **Step 4: Run; expect PASS on both sets.** Commit `feat(core): reach the chain through Esplora or Electrum`.

### Task 3: Watch-only wallet

**Files:** Create `crates/fedimix-core/src/saving/wallet.rs`, `saving/labels.rs`. Modify `store.rs` (migration).

**Interfaces:**
- Produces:
```rust
SavingWallet::create(store_path, DeviceAccount, network) -> Result<SavingWallet>
SavingWallet::open(store_path, network) -> Result<Option<SavingWallet>>
  .sync(&dyn ChainClient) -> Result<SyncSummary>
  .balance() -> Amount;  .in_flight() -> Amount
  .coins(&Labels) -> Vec<Coin>;  .history(&Labels) -> Vec<HistoryItem>
  .next_receive_address() -> Result<String>
  .build_send(SendPlan) -> Result<BuiltTx>      // Task 6
  .build_refill(gap, fee_rate) -> Result<BuiltTx>  // Task 7
```
`DeviceAccount { kind, name, fingerprint, xpub, path }`. `Labels` persists per outpoint: `Tag` (`Unmixed`, `SmallChange`, `Anonymity`, `EarlyExit`), source federation, frozen.

Coins received on a Saving address are `Unmixed`. Coins from `build_send` small change are `SmallChange`. The rest arrive in Phase 4.

- [ ] **Step 1: Failing tests** (using `bdk_wallet::test_utils`-style inserted transactions): `watch_only_has_no_private_keys`, `direct_receive_is_unmixed`, `frozen_is_persisted_and_excluded`, `a_new_address_each_time`, `reopen_keeps_revealed_indices`, `wrong_network_descriptor_is_refused`, `history_nests_change_under_the_send`, `dropped_unconfirmed_tx_leaves_history`.
- [ ] **Step 2: Run; expect FAIL.** Implement. Persist after every apply and every `finish`.
- [ ] **Step 3: Run; expect PASS.** Commit `feat(core): add the watch-only Saving wallet`.

### Task 4: Devices and signing

**Files:** Create `crates/fedimix-core/src/saving/signer.rs`, `saving/devices.rs`, `saving/soft_signer.rs` (`#[cfg(feature = "testing")]`).

**Interfaces:**
- Produces:
```rust
#[async_trait] trait Signer: Send + Sync {
  fn kind(&self) -> DeviceKind;
  async fn fingerprint(&self) -> Result<String>;
  async fn xpub(&self, path: &str) -> Result<String>;
  async fn sign(&self, psbt: &mut Psbt) -> Result<()>;   // rejection maps to DeviceRejected
}
#[async_trait] trait DevicePort: Send + Sync {
  async fn list(&self) -> Result<Vec<DeviceInfo>>;
  async fn connect(&self, id: &str) -> Result<Arc<dyn Signer>>;
}
struct HwiPort;   // enumerates Ledger, Coldcard, Jade, BitBox02
```
Ledger uses the default `wpkh(@0/**)` policy with `hmac = None`. BitBox pairing needs a confirm on the device, and its noise key is stored in `<data_dir>/bitbox`. Compile-only: no hardware here, and the code carries a comment stating that.

- [ ] **Step 1: Failing tests:** `soft_signer_signs_a_built_psbt`, `rejection_maps_to_device_rejected` (fake), `release_build_has_no_soft_signer` (cargo tree check in `scripts/check-no-soft-signer.sh`, run from `just test`).
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Run; expect PASS.** Commit `feat(core): sign through async-hwi`.

### Task 5: RealBackend Saving state

**Files:** Create `crates/fedimix-core/src/real/saving.rs`. Modify `real/mod.rs`, `real/events.rs`, `api.rs`. Test `tests/real_backend.rs`, `tests/devimint.rs`.

**Interfaces:**
- Produces: real `detect_devices`, `setup_saving`, `saving_state`, `receive` for `WalletKind::Saving`, `set_coin_frozen`, `history(Saving, ..)`, Saving sync task, `Event::SavingChanged`, `Features.saving = true`. `RealBackend::with_ports(config, port, chain_factory, devices)`.
- `saving_ready` is true once set up; `moving` now reads as waiting only until Phase 4. `SavingState.device` reports `connected` from the last poll.

- [ ] **Step 1: Failing tests (fake):** `setup_stores_the_account_and_never_a_private_key`, `setup_twice_is_refused`, `reads_do_not_wait_for_the_chain`, `chain_offline_shows_last_balance`, `frozen_survives_restart`, `saving_receive_is_a_fresh_address_labelled_unmixed`, `proxy_applies_to_the_chain`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Devimint test** `saving_sees_a_funded_address`.
- [ ] **Step 4: Run both suites.** Commit `feat(core): add Saving state to RealBackend`.

### Task 6: Send with change to the federation

**Files:** Create `crates/fedimix-core/src/real/saving_send.rs`. Modify `saving/wallet.rs`, `store.rs`. Tests as Task 5.

**Interfaces:**
- Produces: `prepare_send`, `confirm_send` for Saving, `cancel_operation` before Broadcasting.

`prepare_send`: fee rate; `deposit_address()` of the active federation; refuse a recorded address; build with `drain_to`; if change `<= 2 × min_deposit`, rebuild without it (`ChangeDestination::SmallChange`, labelled); no change at all is `None`. `SendPreview.inputs` lists coins with tags; `fee` is the transaction fee. Federation offline: `Offline`, nothing built. No active federation: change stays as `SmallChange` and the preview says so.

`confirm_send`: operation `AwaitingDevice`, `Signing`, `Broadcasting`, `Done`. Verify the signed PSBT still has the previewed outputs and fee before broadcast. Store the signed tx, record the federation address, broadcast, add a deposit watch for the change, write a history row `Sent { change }`. Change deposit arrives in Spending as a Deposit row correlated by outpoint (v2) or watch (v1) so history shows "change → Spending".

- [ ] **Step 1: Failing tests (fake):** `change_goes_to_a_fresh_federation_address`, `small_change_stays_and_is_labelled`, `same_v2_address_is_refused`, `federation_offline_refuses_before_signing`, `no_federation_keeps_change`, `device_rejects_nothing_broadcasts`, `device_missing_stops_at_awaiting_device_and_cancels`, `tampered_psbt_is_refused`, `restart_rebroadcasts_a_signed_tx`, `restart_recognises_a_seen_tx`, `one_saving_operation_at_a_time`, `frozen_coins_are_not_spent`, `above_spendable_is_insufficient`, `max_sends_everything_minus_fee_without_change`, `two_previews_second_fails`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Devimint tests** (software signer): `saving_send_change_lands_in_spending` (both generations, end to end, balance rises by change minus deposit fee), `saving_send_small_change_stays`, `v2_second_send_waits_for_first_change` (generation-specific), `killed_after_sign_rebroadcasts`.
- [ ] **Step 4: Run both suites.** Commit `feat(core): send from Saving with change to the federation`.

### Task 7: Refill

**Files:** Create `crates/fedimix-core/src/real/refill.rs`. Modify `saving/wallet.rs`. Tests as Task 5.

**Interfaces:**
- Produces: `prepare_refill`, `confirm_refill`, `RefillPreview { amount, fee, coins, overshoot, device_name, expires_at }`.

Gap is `target - spending balance`, positive only. Selection: unfrozen confirmed coins, smallest first, grow until `delivered = sum(inputs) - fee >= gap`, one output, no change. Delivered also must exceed `2 × min_deposit`. If all usable coins together cannot cover the gap, refuse with `InsufficientFunds`; a partial refill is not offered. `overshoot = delivered - gap`. Operation path as Task 6; same address rule.

- [ ] **Step 1: Failing tests:** `smallest_first_until_covered`, `delivered_is_net_of_fee`, `one_output_no_change`, `at_or_above_target_is_refused`, `not_enough_coins_is_insufficient`, `frozen_and_unconfirmed_are_skipped`, `refill_needs_the_federation_online`, `same_v2_address_is_refused`, `device_rejection_changes_nothing`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Devimint test** `refill_reaches_spending`, both generations.
- [ ] **Step 4: Run both suites.** Commit `feat(core): refill Spending from Saving`.

### Task 7b: Software Saving wallet

Added 2026-11-05 at the user's request ("Also allow software wallet if user doesn't have hww").

**Files:** Create `crates/fedimix-core/src/saving/seed_signer.rs` (not behind `testing`). Modify `saving/devices.rs`, `seed.rs`, `real/saving.rs`, `api.rs`, `README.md`, `decisions.md`. Tests as Task 5.

**Interfaces:**
- Consumes: `Signer`, `DevicePort`, `DeviceKind::Software`, `Seed`.
- Produces: `SeedSigner` (`Signer` from the recovery words, BIP84 `m/84'/coin'/0'`, coin 0 on bitcoin, 1 elsewhere); a `DevicePort` that lists hardware devices first, then always `DeviceInfo { id: "software", name: "This computer", kind: Software, connected: true }`, with `connect("software")` returning the `SeedSigner`. The `testing` `SoftSigner` (arbitrary xprv, reject hooks) is unchanged and still `testing` only; the doc comment on `DeviceKind::Software` changes to "Software wallet on this computer".

The key comes from the same BIP39 words as Spending (BIP39 seed, empty passphrase), so one backup restores both. Setup, sync, send, refill and stalled handling are unchanged: a software account is an ordinary account whose signer is always connected. Signing skips `AwaitingDevice`; it still runs only from a user's confirm. The extended private key is derived for each call and not kept. No network traffic, so a proxy does not affect it.

- [ ] **Step 1: Failing tests:** `seed_signer_matches_bip84_vector` (words `abandon` ×11 `about`, bitcoin: fingerprint `73c5da0a`, first receive address `bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu`), `software_is_listed_after_devices`, `software_is_listed_when_no_device_answers`, `software_wallet_sets_up_and_sends` (fake chain and port, change to the federation), `software_send_needs_no_awaiting_device`, `release_build_has_no_soft_signer` still passes.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Devimint test** `software_saving_sends_with_change`, both generations, using `SeedSigner` from the backend's own words (not the `testing` signer).
- [ ] **Step 4:** README: the software option, that one phrase restores both wallets, and that a hardware wallet is safer for large amounts. decisions.md: one bullet per choice above. Run both suites. Commit `feat(core): offer a software Saving wallet`.

### Task 8: GTK and real UI

**Files:** Modify `crates/fedimix-gtk/src/ui/{setup_saving,saving_tab,send_dialog,refill_dialog,receive_dialog,settings_page}.rs`, `present.rs`, `scripts/test-real-ui.sh`. Create `scripts/real-flows/saving-*.drive`.

Real mode: Saving tab live, Settings' Hardware wallet and chain server groups visible, Exit and Cover groups stay hidden (`features.scheduler`, `features.cover` stay false). Saving Receive states the address cannot be shown on a device. Setup lists "This computer" (software wallet) after hardware devices, with the sentence "Keys stay on this computer. A hardware wallet is safer for large amounts." The Saving empty state reads "Connect a hardware wallet or use this computer". A software account skips "Connect your <device>". Send and Refill confirm labels name the device; a missing device shows "Connect your <device>" (spec section 4). The `in_flight` line stays hidden until Phase 4.

`test-real-ui.sh` builds with `--features fedimix-gtk/testing`, which adds `--test-signer <xprv-file>` and a device listed as a hardware kind (Ledger), as the core tests do: a `Software` device would skip AwaitingDevice and be taken for "This computer". The flag does not exist in a normal build.

- [ ] **Step 1: Failing tests:** `cli::test_signer_needs_the_feature`, `present::device_cannot_show_address`, `present::refill_offered_only_with_saving_below_target`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Real flows:** set up Saving with the Software device; fund it from `FM_BTC_CLIENT`; send with change, see the change arrive in Spending; refill below target. Read the screenshots.
- [ ] **Step 4: Verify** `just test`, `just interact-demo`, `just test-integration`, `just test-real-ui`, `nix build`, and that `strings` on the nix binary finds no soft signer symbol. Commit `feat(gtk): run Saving on the real backend`.

### Task 9: Close out

- [ ] **Step 1:** README: Saving, supported devices (Ledger, Coldcard, Jade, BitBox02), udev rules, no Trezor, address display limit. status.md "Needs user": hardware testing on each device kind, Trezor, restore-from-words, real-network trial, whether to upstream `next_receive_address`.
- [ ] **Step 2: Commit** `docs: describe Saving`.
