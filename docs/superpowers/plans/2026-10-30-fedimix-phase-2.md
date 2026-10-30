# Fedimix Phase 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Spending real: `fedimix` without `--demo` joins a Fedimint federation, receives and sends via Lightning and on-chain, and shows persistent history, tested against devimint on both module generations.

**Architecture:** `fedimix-core` gains `RealBackend`, which implements the existing `Backend` trait. Its logic talks to a small `FederationPort` trait with owned types, so previews, operations and history are unit-tested with a fake. `FedimintPort` implements that trait over `fedimint-client` and hides the v1/v2 module split; only devimint tests exercise it. SQLite (`store`) holds settings, history, watched operations and joined federations.

**Tech Stack:** Rust 2024, fedimint-* `=0.13.0`, fedimint-rocksdb, rusqlite (bundled), bitcoin 0.32.6, lightning-invoice 0.34, devimint through the Nix flake.

**Spec:** `docs/superpowers/specs/2026-10-30-fedimix-design.md`

**Reference (gitignored, working code and API locations):** `.superpowers/sdd/fedimint-spike.md`, `.superpowers/sdd/fedimint-0.13-notes.md`. Spike source: `/tmp/claude-1000/-home-user-projects-fedimix/00c8fe2b-d6cb-4b31-95aa-9c55dbd4d7d3/scratchpad/fedimint-spike/src/{main,steps}.rs`.

## Global Constraints

- Stay on `master`; no branches or worktrees. Never `git worktree prune` or `git gc`. Never commit `ANALYSIS.md` or `.superpowers/`.
- Before every commit: `nix develop -c just clippy` and `nix develop -c just test` pass. Commit subjects are conventional.
- Core public API stays FFI-friendly: owned types, no generics or borrowed lifetimes in public signatures. No fedimint or bitcoin type crosses `fedimix_core`'s public API.
- `fedimix-core` has no GTK dependency.
- All fedimint-* crates are pinned `=0.13.0`. No patches.
- One client DB per federation; one mnemonic serves all federations (`RootSecret::StandardDoubleDerive`, never double-derive).
- Every `FedimixError` payload is a sentence the UI can show as it is.
- Withdrawal and receive fees are real numbers from the federation; never estimated by the app.
- Nothing under `crates/fedimix-gtk` imports fedimint.
- The Demo backend keeps working: `just interact-demo` stays green.
- Status in `docs/superpowers/status.md` (at most ~30 lines) updated and committed after each task; undecided design goes in `docs/superpowers/decisions.md`.

## Decisions fixed by this plan

Record each in `decisions.md` in the task that implements it.

- **Both module generations.** 17 of 21 listed mainnet federations run v1 mint+wallet; new 0.13 federations default to v2. All tests run against both.
- **RocksDB, not cursed-redb, for client DBs.** `MemAndRedb` keeps the entire database in RAM and is the wasm path; RocksDB is the fedimint default. It costs build time and needs clang.
- **Integration tools live in `devShells.integration`.** devimint needs `--accept-flake-config`; without it nix compiles fedimint from source. Plain `nix develop` must not trigger that.
- **Seed in a 0600 file** `<data_dir>/seed`, as BIP39 words. Spec is silent; a keyring adds a dependency and a failure mode on non-GNOME desktops. Settings shows the words. Restoring from words is not in Phase 2 (listed under Needs user).
- **Network is `CoreConfig.network`** (default bitcoin). A federation on another network is refused at join with `InvalidInvite`.
- **Proxy fails closed.** fedimint 0.13 connectors have no SOCKS5 option. With `socks5_proxy` set, federation connections are not opened: join and open report `Offline` with the state shown, so traffic never goes direct. Spec section 3 cannot be met for federation traffic without an upstream change; listed under Needs user.
- **The active federation stays the first joined** until the anonymity estimator exists (Phase 4).
- **Phase 2 queue.** `moving = balance - target` (saturating), `saving_ready = false`, `QueueState { waiting_for_saving: true, items: [] }`. Saving methods return `NotSetUp("Saving arrives in a later version.")`; `detect_devices` returns `[]`.
- **LNv2 only.** Federations without lnv2 join fine; Lightning methods return `NotSetUp("This federation does not support Lightning.")`.

## Review Focus

1. App killed while a send, invoice or deposit is pending: on restart the watcher resumes and history reaches its final state; balance is never wrong and nothing is paid twice.
2. Federation unreachable at start, or dropping mid-use: the app still opens with last known history, shows Offline, prepares refuse with `Offline`, and it recovers without restart.
3. A Lightning payment that ends `Refunded`, or a gateway that fails: reported as refunded, balance restored, and history says so. Spike saw this happen.
4. Deposit at or below the federation fee: the client never claims it, so the Receive dialog states the minimum.
5. Corrupt or partial state: an invite for a federation already joined, a second app instance on the same data dir, a seed file with wrong permissions, a destination for the wrong network, a federation running unknown extra modules.

---

### Task 1: Phase 1 residuals

Fix before anything real sits behind them. Findings in `.superpowers/sdd/2026-10-30-fedimix-phase-1/final-rereview.md`; rulings in that plan's `progress.md`.

**Files:** Modify `crates/fedimix-gtk/src/{bridge,present,window}.rs`, `crates/fedimix-gtk/src/ui/{send_dialog,receive_dialog,settings_page,app_state}.rs`, `crates/fedimix-core/src/demo/mod.rs`, `docs/superpowers/decisions.md`.

**Interfaces:**
- Produces: `CoreBridge::call` whose `on_done` always runs; a panicked future yields `Err(FedimixError::Internal("Something went wrong inside Fedimix."))`. `call` now requires `Result` output.

- [ ] **Step 1: Write failing tests**
  - `present`: `exit_refused_during_grace_period` (`exit_allowed` false for `Deferred { GracePeriod }`).
  - core: `prepare_exit_refuses_grace_period` on the `exit-deferred` scenario. Add `FedimixError::GracePeriod` (`"Exits wait until the app has been online for a while."`) with its `user_message`.
  - `bridge`: `panicking_future_reports_internal` (run on a glib `MainContext`).
- [ ] **Step 2: Run them; expect FAIL.** `nix develop -c cargo test --workspace`
- [ ] **Step 3: Implement.**
  - N1: refuse in `exit_allowed` and `DemoBackend::prepare_exit`; correct `decisions.md:32`.
  - N2: from `Broadcasting`, after `SEND_LOCK_TIMEOUT = 30 s`, `can_close`/`can_pop` return true and a caption says "You can close this; the result will appear when it arrives". The context watcher reports the outcome by toast.
  - N3: spawn with a held `JoinHandle`, map `JoinError` to `Internal`.
  - N4: failed first snapshot shows an error page with Retry.
  - N5: `Progress` holds weak refs. N6: `#[weak]` in the Retry closure. N7: each Apply writes only its own row, other network values from those in force. N9: description edits bump the generation. N10: after an error, `set_busy(false)` re-applies device state.
  - O5: correct `decisions.md` lines 9, 13 and 17 to current behaviour.
- [ ] **Step 4: Add flow `19-exit-grace.drive`** asserting Exit now is disabled in `exit-deferred` for the grace item. Extend `DemoTiming` so a flow can hold Broadcasting; flow `20-send-stuck.drive` asserts the dialog closes after the timeout (shortened by `DemoTiming`) and the toast still appears.
- [ ] **Step 5: Verify.** `nix develop -c just test`, `nix develop -c just interact-demo`: exit 0.
- [ ] **Step 6: Commit** `fix: settle the Phase 1 review residuals`.

### Task 2: Fedimint in the build, smoke test on both generations

**Files:** Modify `flake.nix`, `Cargo.toml`, `crates/fedimix-core/Cargo.toml`, `justfile`, `README.md`. Create `crates/fedimix-core/tests/devimint.rs`, `scripts/test-integration.sh`.

**Interfaces:**
- Produces: `just test-integration` (runs both module sets, exit 0 only if both pass); test convention: `#[tokio::test(flavor = "multi_thread")] #[ignore = "needs devimint"]`, reading `FM_INVITE_CODE`.

- [ ] **Step 1: Flake.** Keep the existing inputs. Add `fedimint.url = "github:fedimint/fedimint/v0.13.0"` with no `follows`, and `nixConfig` with substituter `https://fedimint.cachix.org`, key `fedimint.cachix.org-1:FpJJjy1iPVlvyv4OMiN5y9+/arFLPcnZhZVVCHCDYTs=`.
  - Default shell and crane `commonArgs` add clang, libclang (`LIBCLANG_PATH`), cmake, protobuf; runpath gains `stdenv.cc.cc.lib`.
  - `devShells.integration` extends the default with `fm.{devimint,fedimintd,fedimint-cli,gatewayd,gateway-cli,fedimint-dbtool,fedimint-recurringd,fedimint-recurringdv2}` and, from `import fedimint.inputs.nixpkgs { overlays = [fedimint.overlays.all]; }`: bitcoind lnd esplora-electrs jq perl procps netcat which; plus `FM_ESPLORA_BASE_EXECUTABLE`.
  - Confirm plain `nix develop -c true` evaluates without building fedimint.
- [ ] **Step 2: Cargo.** Workspace deps per the spike's Cargo.toml (`=0.13.0`), `fedimint-rocksdb`, `rusqlite = { version = "0.32", features = ["bundled"] }`, `bitcoin = "0.32.6"`, `lightning-invoice = "0.34"`. Copy the `[profile.dev.package]` block and `[profile.dev] debug = "line-tables-only"`.
- [ ] **Step 3: Write smoke test `smoke_join_and_read_modules`:** build a client with all five module inits, join `FM_INVITE_CODE` with a fresh mnemonic into a tempdir, assert balance 0.
- [ ] **Step 4: `scripts/test-integration.sh`.** With `set -euo pipefail`, runs twice: `FM_FED_SIZE=1 devimint dev-fed --exec cargo test -p fedimix-core --test devimint -- --ignored --test-threads=1`, once default and once with `FM_ENABLE_MODULE_MINT=1 FM_ENABLE_MODULE_WALLET=1 FM_ENABLE_MODULE_MINTV2=0 FM_ENABLE_MODULE_WALLETV2=0`. `just test-integration` is `nix develop .#integration --accept-flake-config -c scripts/test-integration.sh`.
- [ ] **Step 5: Verify** `just test-integration` from the dev shell, `nix build`, `result/bin/fedimix --demo` start: all exit 0.
- [ ] **Step 6: Commit** `chore: build against fedimint 0.13 and run devimint tests`.

### Task 3: Store, seed and bootstrap

**Files:** Create `crates/fedimix-core/src/{store,seed,config}.rs`. Test inline.

**Interfaces:**
- Produces:
```rust
pub struct CoreConfig { pub data_dir: String, pub network: Network }
pub enum Network { Bitcoin, Signet, Regtest }   // api.rs, public

// crate-private
Store::open(path) -> Result<Store>   // WAL, schema_version table, migrations
Store::settings() / set_settings(&Settings)
Store::federations() / add_federation(&FederationRow) / set_active(&str)
Store::upsert_history(wallet, federation_id, &HistoryItem)
Store::history_page(wallet, before: Option<&str>, limit) -> Vec<HistoryItem>
Store::watches() / add_watch(&Watch) / finish_watch(&id)
Store::cursor(name) / set_cursor(name, u64)
Seed::load_or_create(dir) -> Result<Seed>   // words() -> String, root_secret()
DataDirLock::acquire(dir) -> Result<DataDirLock>
```
`Watch { id, federation_id, kind: WatchKind, created_at }`, `WatchKind::{LnReceive, LnSend, Deposit, Withdraw}`. History rows are newest first by `(timestamp, id)`; `before` is an item id.

- [ ] **Step 1: Failing tests:** `settings_roundtrip_defaults_when_empty`, `reopen_keeps_rows`, `history_pages_are_stable_when_rows_arrive`, `upsert_updates_status_without_moving_the_row`, `seed_file_is_0600_and_stable`, `seed_refuses_group_readable_file` (error sentence, not silently fixed), `seed_refuses_invalid_words`, `second_lock_is_refused`, `newer_schema_is_refused`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** Seed: atomic write through a temp file with mode 0600. Lock: `flock` on `<data_dir>/lock`; second instance gets `Internal("Fedimix is already running with this data folder.")`.
- [ ] **Step 4: Run; expect PASS.** Commit `feat(core): add the store, seed and data dir lock`.

### Task 4: Additive API, demo and GTK for it

**Files:** Modify `crates/fedimix-core/src/{api,backend,handle,destination}.rs`, `demo/*`, `crates/fedimix-gtk/src/ui/{history_list,saving_tab,spending_tab,receive_dialog,settings_page}.rs`, `app_state.rs`. Create `ui/recovery_words.rs`; flows `21-history-more.drive`, `22-recovery-words.drive`.

**Interfaces:**
- Produces, on `Backend` and `Fedimix`:
```rust
async fn history(&self, wallet: WalletKind, before: Option<String>, limit: u32) -> Result<HistoryPage>;
async fn operations(&self) -> Result<Vec<RunningOperation>>;
async fn recovery_words(&self) -> Result<String>;
async fn shutdown(&self) -> Result<()>;
pub struct HistoryPage { pub items: Vec<HistoryItem>, pub more: bool }
pub struct RunningOperation { pub id: OperationId, pub kind: OperationKind, pub state: OperationState }
```
- `ReceiveInfo.min_deposit: Option<Amount>`, `FederationInfo.balance: Amount`, `Event::Operation` gains `history_id: Option<String>`.
- Snapshots keep at most `SNAPSHOT_HISTORY = 50` items.
- `parse_destination` accepts base58 shapes (`1`, `3`, `m`, `n`, `2`). It stays lexical; `validate_address(&str, Network) -> Result<()>` (new, using `bitcoin`, checksum and network) is what `RealBackend` calls.

- [ ] **Step 1: Failing tests:** `history_pages_through_everything_once` (demo), `operations_lists_running_only`, `base58_addresses_parse`, `validate_refuses_wrong_network`, `validate_refuses_bad_checksum`, `present::receive_shows_minimum`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.**
  - History list: "Show more" row appending pages.
  - Receive: "Deposits under N are not credited." under the on-chain address.
  - Settings: "Recovery words" row opens a page with a warning and a Copy button.
  - On startup `app_state` calls `operations()` and registers watchers.
  - The window's close path calls `shutdown` before the app exits.
- [ ] **Step 4: Verify** `just test` and `just interact-demo`. Commit `feat: add paged history, operation listing and recovery words`.

### Task 5: FederationPort and FedimintPort

**Files:** Create `crates/fedimix-core/src/federation/{mod,port,fedimint,onchain,fake}.rs`; extend `tests/devimint.rs`.

**Interfaces:**
- Produces (crate-private; owned, `msat`-based via `Amount`):
```rust
#[async_trait] trait FederationPort: Send + Sync {
  async fn preview(&self, invite: &str) -> Result<FederationPreview>;   // id, name, network, has_lightning
  async fn join(&self, invite: &str, root: &Seed) -> Result<Arc<dyn FederationClient>>;
  async fn open(&self, id: &str, root: &Seed) -> Result<Arc<dyn FederationClient>>;
}
#[async_trait] trait FederationClient: Send + Sync {
  fn id(&self) -> String;  fn name(&self) -> String;
  async fn balance(&self) -> Result<Amount>;
  fn balance_changes(&self) -> BoxStream<'static, Amount>;
  fn online_changes(&self) -> BoxStream<'static, bool>;
  async fn notes(&self) -> Result<Vec<NoteBucket>>;
  async fn min_deposit(&self) -> Result<Amount>;
  async fn lightning_invoice(&self, amount: Amount, description: &str, expiry_secs: u32) -> Result<(String, String)>; // invoice, watch id
  async fn deposit_address(&self) -> Result<(String, Option<String>)>; // address, watch id if the generation has one
  async fn lightning_quote(&self, invoice: &str) -> Result<LightningQuote>;
  async fn pay_invoice(&self, invoice: &str) -> Result<String>;
  async fn withdraw_quote(&self, address: &str, amount: SendAmount) -> Result<WithdrawQuote>;
  async fn withdraw(&self, quote: &WithdrawQuote) -> Result<String>;
  fn watch(&self, watch: Watch) -> BoxStream<'static, WatchUpdate>;
  fn deposits(&self, cursor: u64) -> BoxStream<'static, DepositEvent>; // v2 event log; empty on v1
  async fn shutdown(self: Arc<Self>) -> Result<()>;
}
enum WatchUpdate { Confirming{confirmations:u32,required:u32,amount:Amount}, Settled{amount:Amount,fee:Amount,txid:Option<String>}, Refunded, Expired, Failed{reason:String} }
```
`onchain.rs` holds `enum OnChain { V1(..), V2(..) }` and `enum Mint`, chosen from the config's module kinds at open. A federation with neither wallet returns `NotSetUp("This federation has no on-chain support.")`. `fake.rs` (`#[cfg(test)]`) is scriptable.

v1 deposits report per-address watches with confirmations. v2 reports through `deposits()`, with `await_receive` and a persisted event-log cursor read before `receive()`, as in the spike.

- [ ] **Step 1: Failing devimint tests:** `join_reopen_keeps_identity`, `deposit_credits_balance`, `lightning_receive_credits_balance`, `pay_invoice_settles`, `withdraw_returns_txid`, `min_deposit_is_positive`, `unknown_invite_is_a_sentence`. Helpers `mine(n)`, `fund(address, sat)` use `FM_BTC_CLIENT -rpcwallet=default`; `pay_with_lnd(invoice)` uses `FM_LNCLI` and retries with a new invoice up to 3 times (spike saw timeouts on gateway-pinned invoices).
- [ ] **Step 2: Run** `just test-integration`; expect FAIL.
- [ ] **Step 3: Implement** from the spike's snippets. `ConnectorRegistry` is built once. `online_changes` is true when `connection_status_stream` reports at least a threshold of peers. RocksDB under `<data_dir>/federations/<id>/client`.
- [ ] **Step 4: Run; expect PASS on both sets.** Commit `feat(core): reach federations through FederationPort`.

### Task 6: RealBackend: bootstrap, join, state

**Files:** Create `crates/fedimix-core/src/real/{mod,spending,events}.rs`; test in `tests/real_backend.rs` (fake port) and `tests/devimint.rs`.

**Interfaces:**
- Produces: `pub async fn RealBackend::open(config: CoreConfig) -> Result<Arc<RealBackend>>`, and crate-private `RealBackend::with_port(config, Arc<dyn FederationPort>)`.
- Consumes: Tasks 3-5.

Rules: `open` never needs the network: it reads the store and connects each federation on background tasks, and until then reports `online: false` with last known balances (add `balance_msat` to the federation row). `suggested_federations` returns `[]`. `set_settings` rejects nothing beyond range checks and emits `SettingsChanged`.

- [ ] **Step 1: Failing tests (fake):**
  - `opens_with_an_unreachable_federation_and_recovers`
  - `join_twice_is_idempotent_and_not_an_error`
  - `wrong_network_federation_is_refused`
  - `first_joined_stays_active`
  - `spending_state_splits_at_target` (balance 0.07: for_spending 0.05, moving 0.02)
  - `queue_waits_for_saving`
  - `proxy_set_means_no_connection_attempt`
  - `events_fire_on_balance_and_online_changes`
  - `saving_methods_report_not_set_up`
  - `shutdown_stops_every_task`
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** Devimint test `restart_keeps_federation_and_balance`.
- [ ] **Step 4: Run both suites.** Commit `feat(core): add RealBackend with join and state`.

### Task 7: Receive

**Files:** Create `crates/fedimix-core/src/real/receive.rs`; extend `tests/real_backend.rs`, `tests/devimint.rs`.

**Interfaces:**
- Produces: `Backend::receive` for `WalletKind::Spending`, `ReceiveInfo { payload, uri, expires_at, min_deposit }`. `uri` is `lightning:<invoice>` or `bitcoin:<address>`.
- Consumes: `Store::{add_watch, upsert_history, finish_watch, cursor}`, `FederationClient::{watch, deposits}`.

History mapping: invoice gives `ReceivedLightning`, `Pending`, then `Complete` or `Expired`, row added at creation. Deposit gives `Deposit`, added on first sight, `Confirming` when the generation reports it, then `Complete`. An address alone adds no row. `Notice::DepositConfirmed` on completion of both.

- [ ] **Step 1: Failing tests (fake):** `invoice_settles_into_history`, `unpaid_invoice_expires`, `deposit_walks_through_confirming`, `restart_resumes_watches`, `v2_cursor_is_not_replayed_after_restart`, `deposit_at_or_below_minimum_is_stated`, `receive_while_offline_is_offline`, `no_lightning_module_is_a_sentence`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** All watches respawn from `Store::watches()` in `open`.
- [ ] **Step 4: Devimint tests** `receive_lightning_end_to_end`, `receive_onchain_end_to_end` (history and `Notice` asserted through `subscribe()`), and `killed_mid_deposit_recovers`: drop the backend after funding, mine, reopen, expect `Complete`.
- [ ] **Step 5: Run both suites.** Commit `feat(core): receive via Lightning and on-chain`.

### Task 8: Send

**Files:** Create `crates/fedimix-core/src/real/send.rs`; extend `tests/real_backend.rs`, `tests/devimint.rs`.

**Interfaces:**
- Produces: `prepare_send`, `confirm_send`, `discard_preview`, `cancel_operation`, `operations` for Spending. `PREVIEW_TTL = 600 s`, as the demo. Saving sends return `NotSetUp`.
- Consumes: `validate_address`, `parse_destination`, `FederationClient::{lightning_quote, pay_invoice, withdraw_quote, withdraw, watch}`.

Rules:
- A preview is consumed under the state lock before paying; one preview pays at most once.
- `SendPreview.fee` is the full fee: LN quote; on-chain miner plus federation fee.
- `Max` subtracts the fee. `Exact` above balance gives `InsufficientFunds`.
- `ChangeDestination::None`; `inputs` empty.
- A send may use the whole balance (Phase 4 shrinks the queue).
- State: `Broadcasting` then `Done`, or `Failed { error }`. Refund gives history `Refunded` and `Failed` with `Internal("The payment failed and the money came back.")`.
- `cancel_operation`: known id is accepted and has no effect, because Spending starts at Broadcasting; unknown id is `UnknownItem`.
- The watch row is written before paying, so a crash cannot lose an in-flight payment. The id stored comes from the fedimint op id, and a crash between the write and `pay_invoice` is a watch with no operation, which resolves to `Failed` on restart.

- [ ] **Step 1: Failing tests (fake):** `preview_pays_once`, `expired_preview_refused`, `two_previews_over_the_same_funds_second_fails`, `refund_restores_and_labels`, `send_failure_keeps_history_row_failed`, `max_leaves_nothing_after_fee`, `wrong_network_address_refused`, `expired_invoice_refused`, `amountless_invoice_needs_exact`, `restart_resumes_running_send`, `crash_before_pay_resolves_failed`, `cancel_unknown_errors`, `offline_prepare_is_offline`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Devimint tests** `pay_lightning_end_to_end` (invoice from `FM_LNCLI addinvoice`; balance falls by amount plus previewed fee, within 1 sat), `withdraw_end_to_end` (txid found through `FM_BTC_CLIENT gettransaction`; recipient receives the exact amount), `paying_own_invoice_is_handled`, ending in `Done` or a labelled `Refunded`, never a stuck row.
- [ ] **Step 5: Run both suites.** Commit `feat(core): send via Lightning and on-chain`.

### Task 9: GTK runs on the real backend

**Files:** Modify `crates/fedimix-gtk/src/{main,cli}.rs`, `bridge.rs`, `justfile`, `README.md`. Create `scripts/test-real-ui.sh`, `scripts/real-flows/join-and-receive.drive`.

**Interfaces:**
- Produces: CLI `fedimix [--data-dir PATH] [--network bitcoin|signet|regtest]`. Default data dir is `$XDG_DATA_HOME/fedimix`, else `~/.local/share/fedimix`. `--demo-drive` also works without `--demo`.

`RealBackend::open` runs on the core runtime. Failure shows a dialog with the error sentence and exits 1, without a window that looks usable. Saving tab in real mode shows its empty state, with "Connect a hardware wallet" insensitive and "Available in a later version" beneath it. The Saving setup banner on Spending is hidden while `Setup` is unavailable: add `SavingState.can_set_up: bool`, true in demo.

- [ ] **Step 1: Failing tests:** `cli::real_is_the_default`, `cli::data_dir_and_network_parse`, `present::saving_empty_state_without_setup`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: `scripts/test-real-ui.sh`** inside `devimint dev-fed --exec`, under the sway recipe in `scripts/interact-demo.sh`: start the real binary on a temp data dir with `--network regtest`, drive Join with `FM_INVITE_CODE` (the driver gains `type-env <label> <VAR>`), create a Lightning invoice, read it from a file written by a new driver step `save-text <label> <path>`, pay it with `FM_LNCLI`, `expect` the amount in history, screenshot. Run on the default module set.
- [ ] **Step 4: Second launch** on the same data dir shows the federation and balance. A launch while another runs reports the lock sentence and exits 1.
- [ ] **Step 5: Verify** `just test`, `just interact-demo`, `just test-integration`, `just test-real-ui`, `nix build`: exit 0. If a step cannot run headless, say so in `status.md`.
- [ ] **Step 6: Commit** `feat(gtk): run on the real backend`.

### Task 10: Close out

**Files:** Modify `README.md`, `docs/superpowers/{status,decisions}.md`.

- [ ] **Step 1:** Measure the `--release` build time and binary size; record them. Run a size-4 federation once: `FM_FED_SIZE=4 just test-integration`.
- [ ] **Step 2:** README: real run, data dir, seed file warning, integration commands. status.md "Needs user": real-network trial with small amounts, restore-from-words, SOCKS5 for federation traffic, seed-at-rest choice.
- [ ] **Step 3: Commit** `docs: describe the real backend`.
