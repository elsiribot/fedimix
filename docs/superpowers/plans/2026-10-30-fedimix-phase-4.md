# Fedimix Phase 4 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move money above Spending's target to Saving on its own: a persisted denomination queue, an anonymity estimate counted from the federation's signed session history, Poisson-timed exits to fresh Saving addresses, manual early exits, and privacy labels on every coin an exit makes.

**Architecture:** Two pure modules, `scheduler` (denominations, queue fit, wait and delay maths, readiness) and `anonymity` (counting), carry every rule and are unit-tested with injected clock and RNG. `RealBackend` gains a session sync per connected federation (through a new `FederationClient::session_deposits`), a queue kept in the store, an exit path that reuses the Phase 2 intent machinery under its own `WatchKind::Exit`, and one scheduler task. Saving reads exit labels by address, so a coin is labelled the moment it appears.

**Tech Stack:** fedimint 0.13.0 (`fedimint-api-client` session API, wallet v1/v2 and LNv2 common types), rusqlite 0.31, bdk_wallet 3.2.0, rand 0.8, GTK4/libadwaita.

**Spec:** `docs/superpowers/specs/2026-10-30-fedimix-design.md` (sections 1 "Exit timing", "Privacy records", 2 "Moving to Saving", "Coins", 3 `anonymity`/`scheduler`, 4).

**Reference (gitignored):** `.superpowers/sdd/fedimint-0.13-notes.md` §3.3-3.7, `.superpowers/sdd/fedimint-spike.md` §5-7. Fedimint source: `/tmp/claude-1000/-home-user-projects-fedimix/00c8fe2b-d6cb-4b31-95aa-9c55dbd4d7d3/scratchpad/fedimint-research/fedimint` (tag v0.13.0); paths below are relative to it.

## Global Constraints

- Denominations: "1-2-5 series from 0.001 to 1 BTC (0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1). Greedy split, largest first." A remainder under 0.001 BTC stays in Spending.
- Defaults (Settings already holds them): target 0.05 BTC, anonymity target 50, minimum time in pool 24 h, online grace 10 min, fee guard 2% of denomination. `features.cover` stays false: cover traffic is Phase 5.
- Anonymity estimate: "number of federation deposits (on-chain deposit inputs and Lightning receives) with amount ≥ that denomination, accepted since the user's deposit."
- "The exit rule must not depend on other users withdrawing the same amount first." The anonymity target "is a soft target, not a gate."
- "Online grace period: no exit … within 10 minutes of the app starting or reconnecting to the federation." No override, manual exits included (Phase 1 ruling N1).
- "Each denomination draws its own delay. Denominations from the same deposit never exit together." Delays "are redrawn on every start (Poisson is memoryless)". "Clock and RNG are injected."
- "Fee guard: automatic exits are deferred while the withdrawal fee exceeds 2% of the denomination. Manual early exits are not blocked; they show the fee."
- "Withdrawal fees: paid from the Spending balance. If Spending cannot cover the fee for an exit, the exit waits and Spending shows a hint."
- "Exits always use a freshly derived Saving address and are never retried to a previously used one."
- Each Saving UTXO from an exit carries anonymity estimate at exit, source federation, and the early-exit flag; direct receives stay unmixed, small change stays small change.
- "Federation unreachable: … scheduler paused; grace period restarts on reconnect." Exits need no device.
- Never charge more than the preview shows (an exit is re-quoted at submission; a higher fee is `PreviewExpired`).
- Core public API stays owned and FFI-friendly; no fedimint, bitcoin or bdk type crosses it or reaches `crates/fedimix-gtk`.
- Phase 1 ruling O1, binding: Spending may not spend queued notes silently. Spendable for a send is the whole balance; a send shrinks the queue from the most recent item, and its review says so.
- Phase 3 carry-overs, binding: exits have their own `WatchKind::Exit`; `SavingState.in_flight` is redefined as exits on their way; overshoot above target from refills rejoins the exit queue.
- Git: work on `master` in `/home/user/projects/fedimix`. One implementer at a time in the main tree. Tasks marked **[parallel]** may run at the same time in a worktree: `git worktree add ../fedimix-wt-<task> master`, `CARGO_TARGET_DIR=../fedimix-wt-<task>/target`, rebase onto `master` when done, then `git worktree remove ../fedimix-wt-<task>`. Never `git worktree prune`, never `git gc`.
- `git add` only the paths a task names. Never commit `ANALYSIS.md`, `coin.rs` or `.superpowers/`. Conventional commit subjects, ending with the Co-Authored-By line.
- Gates before each task's commit: `nix develop -c just clippy`, `nix develop -c just test`, `just test-integration` (both module generations), `just test-real-ui`, and `just interact-demo` for tasks that touch UI or demo. `nix build` runs once, in Task 11.
- Each task updates `docs/superpowers/status.md` (at most ~30 lines) and adds one `decisions.md` bullet per choice beyond this plan. A worktree task puts those two files in its last commit alone, so a rebase conflict stays in that one commit.

## Fedimint facts pinned from source

Anonymity, readable by a client on both generations:
- `client.api().session_count()` is the number of completed sessions (`fedimint-api-client/src/api/mod.rs:503`). `get_session_status(idx, decoders, core_api_version, Some(&broadcast_public_keys))` (`:495`) returns `SessionStatus::Complete(SessionOutcome)` with the guardians' threshold signature checked (`api/global_api/with_cache.rs:155-200`, `SignedSessionOutcome::verify`). The v2 path decodes with exactly the registry passed (`with_cache.rs:178`), so pass `client.decoders().clone().with_fallback()` (`fedimint-core/src/module/registry.rs:170`): items of modules Fedimix does not register then decode as unknown, as fedimint's own recovery does (`fedimint-client-module/src/module/init/recovery.rs:287`). Task 1 checks this.
- v1 deposit: an input downcasting to `fedimint_wallet_common::WalletInput` (`modules/fedimint-wallet-common/src/lib.rs:315-385`): `V0(proof)` gives `proof.tx_output().value` and `proof.outpoint()`; `V1 { outpoint, tx_out, .. }` gives `tx_out.value`. Gross on-chain value.
- v2 deposit: `fedimint_walletv2_common::WalletInput::V0(WalletInputV0 { output_index, fee, .. })` has no amount (`modules/fedimint-walletv2-common/src/lib.rs:170`). The module endpoint `output_info_slice(start, end)` returns `OutputInfo { index, value, spent, outpoint }` (`lib.rs:100`; server `modules/fedimint-walletv2-server/src/lib.rs:889`, `:1381`), spent outputs included, no range limit. The typed API trait is private, so call it raw: `client.api().with_module(input.module_instance_id()).request_current_consensus(OUTPUT_INFO_SLICE_ENDPOINT.to_string(), ApiRequestErased::new((i, i + 1)))`. Observed in the spike (§6): value 100_000 sat for a 100_000 sat deposit.
- Lightning receive: an output downcasting to `fedimint_lnv2_common::LightningOutput::V0(LightningOutputV0::Incoming(c))`, amount `c.commitment.amount` (`modules/fedimint-lnv2-common/src/lib.rs:85`, `contracts.rs:23-31`), which is net of the gateway fee. It cannot be told from a gateway-to-gateway swap; it counts.
- Not countable, accepted: LNv1 receives (Fedimix registers no LNv1 client; they decode as unknown), ecash moving between users (mint-only transactions).

Withdrawal to a Saving address (already built in `crates/fedimix-core/src/federation/onchain.rs:165-301`):
- v1: fee is `get_withdraw_fees(&addr, amount) -> PegOutFees` (`modules/fedimint-wallet-client/src/lib.rs:958`) plus the mint's `send_fee_quote(amount + peg-out fee)`. Submit `withdraw(&addr, amount, fees, meta)` (`:1922`); `subscribe_withdraw_updates` ends in `WithdrawState::Succeeded(txid)` (`:2016`) once the federation signed it.
- v2: fee is `send_fee()` (miner, `modules/fedimint-walletv2-client/src/lib.rs:270`) plus `send_fee_quote(amount + miner)` (`:292`). Submit `send(addr, amount, Some(miner), meta)` (`:382`); `await_final_send_operation_state` gives `Success(txid)` (`:474`), observed at broadcast with 0 blocks.
- On both the fee comes on top: the output to the Saving address is exactly the denomination. `WithdrawQuote.fee` is miner plus federation fee, and is what the fee guard compares.
- Correlation: the exit's address is revealed fresh and written with the item before submission. The Saving coin at that address is the exit; `Settled.txid` is stored and compared with the coin's txid (a mismatch is logged and the coin is still the exit's, as only the federation knows the address).

## Rulings fixed by this plan

Record each in `decisions.md` in the task that implements it.

- **Amounts compared**: an on-chain deposit's gross value, a Lightning receive's contract amount, each `>=` the denomination.
- **Strictly after**: an item counts sessions with index `> origin.session`; its own session never counts.
- **Own deposits do not count**: on-chain ones by outpoint (v2 `DepositEvent.id`) or txid (v1 row `txid`); Lightning ones are subtracted by count, using `ReceivedLightning` rows that now record their session. Result saturates at 0.
- **Origin** of a new item: the latest `Complete` `Deposit` or `ReceivedLightning` row of that federation with a session (its `session` and `timestamp`); none, the current session count and now. Minimum time in pool runs from `origin.at`.
- **Queue fit**: the queue is booked money. It grows when `balance - target - queued >= 0.001 BTC`. It shrinks, newest first, only on a Spending send's confirm and on a target change, to `queued <= max(0, balance_after - target)`; the freed money above the cap that is left is split again at once, with the newest removed item's origin. Exit fees never shrink it: they come from the part for spending. Always `queued <= balance`.
- **Wait**: `mean_wait = clamp(short_wait × (target / max(estimate, 1))², short_wait, max_wait)`, `short_wait = 3 h`, `max_wait = 30 days`. Delay `ceil(-ln(1 - u) × mean)`, `u` uniform in `[0, 1)`. Redrawn on start, reconnect, a settings change, and whenever an item's estimate changes.
- **Spacing**: no two automatic exits of one federation within 1 h, whatever their deposit. A manual exit ignores it but counts for later automatic ones.
- **Statuses**, first match: Saving not usable (`waiting_for_saving`) or younger than minimum time → `Queued`; federation offline or no fee quote → `Deferred{Offline}`; in grace → `Deferred{GracePeriod}`; fee > guard → `Deferred{FeeGuard}`; for spending < fee → `Deferred{FeeUnaffordable}`; else `Scheduled{at}`. The real backend never reports `Eligible`. `Landed` items leave `QueueState`.
- **Landed** means the exit's output has 1 confirmation in Saving. `SavingState.in_flight` is the sum of denominations of `Withdrawing` exits until then. `held_back` keeps using `SavingWallet::in_flight()` (unconfirmed coins from others).
- **Failed exit**: the item returns to `Queued` with the same origin; its address stays recorded (so a late payment there is still labelled) and is never used again.
- **Attainable**: `current + round(rate × expected_secs / session_secs)`, where `rate` is own-excluded deposits `>= D` per session over the last 480 synced sessions, `expected_secs = max(0, origin.at + min_time - now) + mean_wait(current)`, and `session_secs` is measured on this connection once 10 sessions have passed, otherwise 180. `QueueState.anonymity_estimate` and `typical_exit_secs` are the medians over items; 0 when empty.
- **Federation estimate**: `FederationInfo.anonymity_estimate` is own-excluded deposits `>= 0.001 BTC` in its last 480 synced sessions. Settings marks the largest "Most private". Fedimix does not switch the active federation by itself (decisions.md:83).
- **Sync start**: cursor `anon:<federation>`, else `joined_session`, else the smallest open item origin, else the current count. A session that fails is retried, never skipped, backoff 1 s doubling to 5 min.

## Review Focus

1. A Spending send confirmed while an exit of the same federation is submitting: neither may overdraw. Both go through `Core::reserve`, and the send's fit only removes `Queued` items, never a `Withdrawing` one. Test in Task 6.
2. A crash after the exit's row and address are written but before or after `withdraw`: restart must yield exactly one withdrawal and a new address on retry. Tests in Task 6.
3. A federation whose connection flaps: each reconnect restarts the grace period and redraws; nothing exits inside it, by hand or not. Test in Task 7.
4. Saving scanning, broken or not set up when an exit is due or lands: due items wait as `Queued` with `waiting_for_saving`; a coin that lands during a scan is labelled when the scan ends, as labels are read by address. Tests in Tasks 5 and 6.
5. One session the client cannot fetch or decode: counting for that federation stops there, estimates keep their last values, and exits still happen (slower). Test in Task 4.

---

### Task 1: Spike, session decoding [parallel with Task 2]

**Files:** none in the repo except `docs/superpowers/decisions.md`. Code goes in the scratchpad spike (`.../scratchpad/fedimint-spike`).

**Question:** With Fedimix's module set (lnv2, mint, mintv2, wallet, walletv2; no meta, no LNv1), does `get_session_status(i, &client.decoders().clone().with_fallback(), client.core_api_version().await, cfg.global.broadcast_public_keys.as_ref())` return `Complete` for every `i < session_count()` on devimint of both generations, and do the downcasts in "Fedimint facts" find exactly one on-chain deposit of 100_000 sat (gross, with its funding outpoint) and one LNv2 incoming contract after a 5_000 sat receive?

- [ ] **Step 1:** In each generation, from a fresh join: deposit 100_000 sat, receive 5_000 sat over Lightning from LND, mine, wait for both to settle, then fetch every session and print the deposits found and the ms per session.
- [ ] **Step 2: Exit criterion.** Both generations yes: Task 4 as written. A decode error on any session: also register `fedimint_meta_client::MetaClientInit` and fedimint-ln-client's `LightningClientInit` (decoders only) and rerun; still failing: stop and report BLOCKED with the error. Record the answer and timings in one decisions.md bullet. Commit `docs: record how sessions decode`.

### Task 2: Scheduler and anonymity rules [parallel with Task 1]

**Files:** Create `crates/fedimix-core/src/scheduler.rs`, `crates/fedimix-core/src/anonymity.rs`. Modify `lib.rs` (modules; `SessionDeposit`, `DepositSource` into `testing`), `demo/mod.rs` (use `scheduler::split` and `DENOMINATIONS_SAT` instead of its own copy).

**Interfaces:**
- Produces (`scheduler`, crate-private):
```rust
pub(crate) const DENOMINATIONS_SAT: [u64; 10]; // 100_000_000 down to 100_000
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExitTiming { pub short_wait_secs: u64, pub max_wait_secs: u64, pub spacing_secs: u64, pub tick_secs: u64, pub session_poll_secs: u64 }
impl ExitTiming { pub(crate) const DEFAULT: Self /* 10_800, 2_592_000, 3_600, 30, 30 */; pub(crate) const FAST: Self /* 20, 120, 30, 2, 2 */; }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) struct Origin { pub session: u64, pub at: Timestamp }
pub(crate) struct Booked { pub id: String, pub denomination: Amount, pub origin: Origin } // Queued items, oldest first
pub(crate) fn split(amount: Amount) -> Vec<Amount>;
pub(crate) fn grow(queued: Amount, balance: Amount, target: Amount) -> Vec<Amount>;
pub(crate) struct Fit { pub removed: Vec<String>, pub regrow: Vec<Amount>, pub origin: Option<Origin> }
pub(crate) fn fit(booked: &[Booked], cap: Amount) -> Fit;
pub(crate) fn mean_wait_secs(estimate: u32, target: u32, timing: &ExitTiming) -> u64;
pub(crate) fn draw_delay(mean_secs: u64, uniform: f64) -> u64;
pub(crate) struct Conditions { pub now: Timestamp, pub saving_usable: bool, pub online: bool, pub online_since: Option<Timestamp>, pub fee: Option<Amount>, pub for_spending: Amount, pub settings: Settings }
pub(crate) enum Readiness { Waiting(QueueItemStatus), Ready }
pub(crate) fn readiness(origin_at: Timestamp, denomination: Amount, c: &Conditions) -> Readiness;
pub(crate) fn not_before(last_automatic_exit: Option<Timestamp>, timing: &ExitTiming) -> Option<Timestamp>;
pub(crate) fn attainable(current: u32, per_session: f64, expected_secs: u64, session_secs: u64) -> u32;
pub(crate) fn median(values: &mut [u64]) -> u64; // 0 when empty, lower middle for even counts
```
- Produces (`anonymity`):
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum DepositSource { OnChain, Lightning }
#[derive(Debug, Clone, PartialEq)] pub struct SessionDeposit { pub source: DepositSource, pub amount: Amount, pub reference: Option<String> } // "txid:vout" on chain
#[derive(Default)] pub(crate) struct Own { pub outpoints: HashSet<String>, pub txids: HashSet<String>, pub lightning: Vec<(u64, Amount)> }
pub(crate) const FEDERATION_WINDOW_SESSIONS: u64 = 480;
pub(crate) const DEFAULT_SESSION_SECS: u64 = 180;
pub(crate) fn estimate(deposits: &[(u64, SessionDeposit)], after: u64, denomination: Amount, own: &Own) -> u32;
pub(crate) fn per_session(deposits: &[(u64, SessionDeposit)], head: u64, denomination: Amount, own: &Own) -> f64; // over min(480, synced) sessions ending at head
```

- [ ] **Step 1: Failing tests** (in-module):
  - `split_is_greedy_largest_first`: 0.42 BTC → [0.2, 0.2, 0.02]; 0.0137 → [0.01, 0.002, 0.001]; 99_999 sat → [].
  - `grow_queues_only_whole_denominations_above_target`: queued 0, balance 0.47, target 0.05 → [0.2, 0.2, 0.02]; balance 0.0509 → [].
  - `a_send_shrinks_the_queue_from_the_newest_item`: booked [a 0.2, b 0.2, c 0.02], cap 0.389997 BTC → removed [c, b], regrow `split(0.189997)` = [0.1, 0.05, 0.02, 0.01, 0.005, 0.002, 0.002], origin = c's.
  - `mean_wait_is_short_at_the_target`: (50, 50) → 10_800; `mean_wait_quadruples_at_half_the_target`: (25, 50) → 43_200; `mean_wait_is_capped_with_no_deposits`: (0, 50) → 2_592_000; `mean_wait_does_not_shrink_past_the_target`: (500, 50) → 10_800.
  - `delay_is_the_exponential_quantile`: `draw_delay(10_800, 0.0) == 0`, `draw_delay(10_800, 0.5) == 7_486`; `delays_average_their_mean`: 10_000 draws from `StdRng::seed_from_u64(7)` average within 2% of 10_800.
  - `readiness_follows_the_order`: one assert per status with `Settings::default()`: no Saving → `Queued`; origin 86_399 s ago → `Queued`; offline → `Deferred{Offline}`; online 599 s → `Deferred{GracePeriod}`; denomination 100_000 sat with fee 2_001 sat → `Deferred{FeeGuard}`, 2_000 sat → not; for spending 1_000 sat, fee 1_500 sat → `Deferred{FeeUnaffordable}`; else `Ready`.
  - `automatic_exits_are_an_hour_apart`: last at 1_000 → `Some(4_600)`; none → `None`.
  - `attainable_adds_the_deposits_expected_meanwhile`: (10, 0.2, 3_600, 180) → 14.
  - Anonymity: `counts_deposits_at_or_above_the_denomination` (fixture sessions 10-14 with 0.0005, 0.001, 0.01, 0.02 BTC on chain and a 0.01 BTC Lightning; D = 0.01, after 9 → 3), `the_origin_session_is_not_counted` (after 12 → only 13-14), `own_deposits_do_not_count` (an outpoint, a txid and one own Lightning row each remove one; never below 0), `per_session_uses_the_recent_window` (600 synced sessions, 480 counted).
- [ ] **Step 2: Run** `nix develop -c cargo test -p fedimix-core scheduler anonymity`; expect FAIL (not defined).
- [ ] **Step 3: Implement.** Pure functions only; no I/O, no clock, no RNG inside. `fit` removes from the end of `booked` while its total exceeds `cap`; `regrow = split(min(cap - kept, removed_total))`.
- [ ] **Step 4: Run; expect PASS.** Gates. Commit `feat(core): add the exit queue and anonymity rules`.

### Task 3: Store, labels and Saving rows

**Files:** Modify `crates/fedimix-core/src/store.rs` (schema 10), `saving/labels.rs`, `saving/wallet.rs`, `real/receive.rs`, `real/saving.rs` (source federation name).

**Interfaces:**
- Consumes: `Origin`, `SessionDeposit` (Task 2).
- Produces:
```rust
pub enum WatchKind { LnReceive, LnSend, Deposit, Withdraw, Exit }  // "exit"
pub(crate) enum ExitState { Queued, Withdrawing, Landed }
pub(crate) struct ExitItem { pub id: String, pub federation_id: String, pub denomination: Amount, pub origin: Origin, pub created_at: Timestamp, pub state: ExitState, pub intent: Option<String>, pub address: Option<String>, pub txid: Option<String>, pub fee: Option<Amount>, pub estimate: Option<u32>, pub early: bool }
pub(crate) struct ExitStart { pub item_id: String, pub address: String, pub estimate: u32, pub early: bool, pub fee: Amount, pub row: HistoryItem, pub watch: Watch }
Store::exit_items(&self, federation: Option<&str>) -> Result<Vec<ExitItem>>   // not Landed, oldest first
Store::add_exit_items(&self, items: &[ExitItem]) -> Result<()>
Store::remove_queued_exits(&self, ids: &[String]) -> Result<()>                // skips any not Queued
Store::begin_exit(&self, start: &ExitStart) -> Result<()>                     // item Withdrawing + address + row + watch, one write
Store::exit_by_intent(&self, intent: &str) -> Result<Option<ExitItem>>
Store::exit_settled(&self, item_id: &str, txid: &str) -> Result<()>
Store::requeue_exit(&self, item_id: &str) -> Result<()>                       // clears intent/address/txid, keeps origin
Store::exit_landed(&self, item_id: &str) -> Result<()>
Store::add_session(&self, federation: &str, session: u64, deposits: &[SessionDeposit]) -> Result<()> // sets cursor anon:<federation> = session + 1 in the same write
Store::session_deposits(&self, federation: &str, after: u64) -> Result<Vec<(u64, SessionDeposit)>>
Store::session_cursor(&self, federation: &str) -> Result<Option<u64>>
pub struct ExitLabel { pub item_id: String, pub federation_id: String, pub denomination: Amount, pub estimate: u32, pub early: bool }
Labels::exit_at(&self, address: &str) -> Option<&ExitLabel>
```
Schema 10 adds `exit_items (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE, …)` (newest = highest `seq`), `exit_addresses (address PRIMARY KEY, item_id, federation_id, denomination_msat, estimate, early)`, and `anon_deposits (federation_id, session, position, source, amount_msat, reference, PRIMARY KEY (federation_id, session, position))`. `Store::sends` lists `Exit` watches too; `is_send` stays send-only.

Coin tag precedence: an outpoint label's tag, then `exit_at(address)` (`Anonymity{estimate}`, or `EarlyExit{estimate}` when early), then `Unmixed`. `Coin.source_federation` is the federation's name, or its id when no row names it. A received transaction with an output at an exit address is `FromSpending { denomination }` with that output's amount. `ReceivedLightning` rows record `session` when they settle, as deposits do.

- [ ] **Step 1: Failing tests:** `a_version_9_database_gains_the_exit_tables`, `exit_items_keep_their_order_and_origin`, `begin_exit_writes_item_address_row_and_watch_together` (a trigger refusing the watch leaves none of them), `requeue_keeps_the_address_label`, `remove_skips_a_withdrawing_item`, `a_session_and_its_cursor_are_one_write`, `exit_watches_count_as_sends_for_resolving`, `an_exit_coin_is_tagged_by_its_address`, `an_early_exit_coin_is_flagged`, `an_exit_reads_from_spending_in_saving_history`, `direct_receives_stay_unmixed`, `a_lightning_receive_keeps_its_session`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Run; expect PASS.** Gates. Commit `feat(core): store the exit queue, session deposits and exit labels`.

### Task 4: Session sync and estimates

**Files:** Create `crates/fedimix-core/src/federation/sessions.rs`, `crates/fedimix-core/src/real/anonymity.rs`. Modify `federation/{port,fedimint,fake,mod}.rs`, `real/{events,mod}.rs`. Test `tests/real_backend.rs`, `tests/devimint.rs`.

**Interfaces:**
- Consumes: Task 1's answer; `anonymity::*` (Task 2); `Store::add_session`, `session_deposits`, `session_cursor` (Task 3).
- Produces:
```rust
// FederationClient
/// What a completed session accepted: wallet deposit inputs of either generation and LNv2 incoming contracts. `None` while it is not complete.
async fn session_deposits(&self, session: u64) -> Result<Option<Vec<SessionDeposit>>>;
FakeClient::push_session(&self, deposits: Vec<SessionDeposit>)   // completes the next session; session_count follows
FakeClient::fail_session(&self, session: u64, error: FedimixError)
pub(super) async fn follow_sessions(core: Arc<Core>, client: Arc<dyn FederationClient>, id: String)
Core::own_deposits(&self, federation: &str) -> Result<Own>
Core::item_estimate(&self, federation: &str, origin: Origin, denomination: Amount) -> Result<u32>
Core::federation_estimate(&self, federation: &str) -> u32
```
`follow_sessions` runs as a task in the connection's `Watches` (it stops with the connection), fetches from the start session (rulings) up to `session_count()`, polls each `session_poll_secs`, and emits `QueueChanged` (and `FederationChanged` when the federation estimate moved) after a batch. A v2 `output_info_slice` failure fails that session's fetch. `FederationInfo.anonymity_estimate` is `federation_estimate`.

- [ ] **Step 1: Failing tests (fake):** `sync_starts_at_the_join`, `sync_resumes_from_its_cursor_after_restart`, `own_deposits_are_left_out_of_the_count`, `federation_estimate_counts_the_recent_window`, `an_undecodable_session_stalls_counting_but_not_exits` (Review Focus 5: `fail_session(3)`; sessions 4+ not stored; estimate keeps its value; the retry after the error clears counts them).
- [ ] **Step 2: Run; expect FAIL.** Implement from "Fedimint facts" and Task 1.
- [ ] **Step 3: Devimint tests,** both generations: `session_deposits_see_a_deposit_and_a_lightning_receive` (100_000 sat gross with its outpoint; a contract ≥ 4_900 sat for a 5_000 sat receive), `another_wallets_deposit_raises_the_estimate` (a second `FedimintPort` client on another seed deposits 200_000 sat; an estimate for 0.001 BTC after the first wallet's deposit rises by 1, the first wallet's own deposit not counted).
- [ ] **Step 4:** Gates. Commit `feat(core): count deposits from the federation's signed sessions`.

### Task 5: The queue in RealBackend

**Files:** Create `crates/fedimix-core/src/real/queue.rs`. Modify `real/{mod,spending,send,events,saving}.rs`. Test `tests/real_backend.rs`.

**Interfaces:**
- Consumes: `grow`, `fit`, `split`, `Origin`, `mean_wait_secs`, `attainable`, `median` (Task 2); store queue functions (Task 3); `item_estimate`, `per_session` data (Task 4).
- Produces:
```rust
Core::tend_queue(&self, federation: &str, balance: Amount, session_count: u64) -> Result<bool> // grow, and clamp to queued <= balance
Core::queued_total(&self, federation: &str) -> Result<Amount>                               // Queued items only
Core::queue_shrink_for(&self, federation: &str, debit: Amount, balance: Amount) -> Result<Amount> // what a debit would take from the queue
Core::fit_queue(&self, federation: &str, cap: Amount) -> Result<()>
Core::queue_state(&self) -> Result<QueueState>                                             // active federation
Core::saving_usable(&self) -> bool                                                          // set up, wallet open, first scan done
```
`tend_queue` runs after each balance change of a connected federation (`forward_balance`, reading `session_count` once), and on each scheduler tick (Task 7). `SpendingState.moving` is `queued_total` and `for_spending = balance - moving`. A Spending `prepare_send` whose debit would shrink the queue adds the note `"{X} of this comes from money moving to Saving."` (X = `queue_shrink_for`), and `confirm_send` fits the queue to `max(0, balance - debit - target)` after its row is written. `set_settings` fits the queue when the target rises. `QueueState.items` carries each item's status (Task 7 fills `Scheduled`; until then ready items read `Queued`), `anonymity_current`, `anonymity_attainable`; `waiting_for_saving = !saving_usable()`. `SavingState.in_flight` is the sum of `Withdrawing` denominations not landed.

- [ ] **Step 1: Failing tests (fake):** `deposits_above_target_are_queued_in_denominations` (0.47 BTC, target 0.05 → 0.2, 0.2, 0.02; moving 0.42; for spending 0.05), `a_remainder_below_the_smallest_stays_in_spending`, `refill_overshoot_rejoins_the_queue` (origin = the refill's Deposit row session), `a_send_takes_from_the_queue_newest_first`, `the_review_says_when_a_send_takes_from_the_queue`, `raising_the_target_shrinks_the_queue`, `queue_follows_the_active_federation`, `without_saving_the_queue_waits` (Review Focus 4: not set up, broken, scanning each give `waiting_for_saving` and all `Queued`), `in_flight_counts_exits_until_they_confirm` (with a store-written `Withdrawing` item), `held_back_still_counts_unconfirmed_receives`, `queue_survives_restart`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Run; expect PASS.** Gates. Commit `feat(core): keep money above the target in a denomination queue`.

### Task 6: Exits and early exit

**Files:** Create `crates/fedimix-core/src/real/exit.rs`. Modify `real/{mod,send,receive,saving}.rs`, `saving/wallet.rs` (`peek_receive_address`). Test `tests/real_backend.rs`, `tests/devimint.rs`.

**Interfaces:**
- Consumes: Tasks 3-5; Phase 2's `withdraw_quote`, `withdraw`, `find_operation`, `reserve`, `fresh_quote` rule.
- Produces:
```rust
pub(super) fn is_outgoing(kind: WatchKind) -> bool            // LnSend | Withdraw | Exit; replaces is_send at its call sites
pub(super) fn operation_kind(kind: WatchKind) -> OperationKind // Exit -> Exit, else Send
Core::prepare_exit(&self, item_id: &str) -> Result<ExitPreview>
Core::exit_now(self: &Arc<Self>, preview_id: &str) -> Result<OperationId>
Core::start_exit(self: &Arc<Self>, item: &ExitItem, quote: WithdrawQuote, early: bool) -> Result<OperationId> // Task 7 calls this too
Core::apply_exit(&self, watch: &Watch, update: &WatchUpdate) -> Result<()>
Core::land_exits(&self, wallet: &SavingWallet) -> Result<()>  // after each Saving sync
SavingWallet::peek_receive_address(&self) -> Result<String>    // for quotes; reveals nothing
```
`report`, `finish_row` and `running_sends` take the kind from the watch, so exits report `OperationKind::Exit`, also after restart. `prepare_exit` quotes `Exact(denomination)` to a peeked address and refuses: unknown/withdrawing item `UnknownItem`; grace `GracePeriod`; Saving not usable `NotSetUp("Set up Saving first.")`; offline `Offline`; for spending below fee `InsufficientFunds`. Not refused: fee guard, minimum time, spacing. `start_exit` re-quotes (higher fee: `PreviewExpired`), reveals a fresh address with `next_receive_address`, reserves `denomination + fee`, writes `ExitStart` (row `ToSaving { denomination }`, amount = denomination, fee, `Pending`; watch `Exit` keyed on the intent; estimate = `item_estimate` now), reports `Broadcasting`, submits in its own task. `Settled` → row `Complete` with txid, `exit_settled`, `Done`. `Failed`/`Refunded`/`Expired` → row `Failed`, `requeue_exit`, `Failed`. Resolver: found resumes; not found → `requeue_exit` and row `Failed` with "This exit was never sent, so no money left Spending." `land_exits`: for each `Withdrawing` item whose address holds a coin with 1 confirmation of exactly its denomination → `exit_landed`, `Notice::ExitLanded { amount }`, `QueueChanged`, `SavingChanged`.

- [ ] **Step 1: Failing tests (fake chain, fake port, software Saving):** `exit_now_pays_a_fresh_saving_address` (address unseen before, quote amount = denomination), `an_exit_reports_as_an_exit` (events and `operations()` after restart say `Exit`), `exit_now_shows_the_fee_and_ignores_the_fee_guard`, `exit_now_is_refused_in_the_grace_period`, `exit_now_never_charges_more_than_shown`, `an_exit_lands_labelled_as_early` (`EarlyExit{estimate}`, source federation name, Saving row `FromSpending`), `a_failed_exit_requeues_and_never_reuses_its_address`, `killed_before_submit_requeues_with_a_new_address` and `killed_after_submit_resumes_the_exit` (Review Focus 2, `halt_sends`), `a_send_and_an_exit_never_overdraw` (Review Focus 1: send previewed over the whole balance while an exit is submitting → `InsufficientFunds`; the fit leaves the `Withdrawing` item), `a_coin_landing_during_a_scan_is_labelled_after_it` (Review Focus 4).
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Devimint test,** both generations: `exit_now_lands_in_saving_labelled` (software Saving; Spending funded 0.06 BTC, target 0.05 → one 0.01 item; exit now; Saving holds a 0.01 BTC coin tagged `EarlyExit`, Spending row `Complete` whose txid is the coin's; Spending debited by 0.01 BTC plus at most the previewed fee).
- [ ] **Step 4:** Gates. Commit `feat(core): exit queued money to Saving`.

### Task 7: Automatic exits

**Files:** Create `crates/fedimix-core/src/real/scheduler.rs`. Modify `real/{mod,events,exit}.rs`, `lib.rs` (`testing`). Test `tests/real_backend.rs`, `tests/devimint.rs`.

**Interfaces:**
- Consumes: `readiness`, `mean_wait_secs`, `draw_delay`, `not_before`, `ExitTiming` (Task 2); `tend_queue` (Task 5); `start_exit` (Task 6).
- Produces:
```rust
pub type Draw = Arc<dyn Fn() -> f64 + Send + Sync>;                 // uniform [0, 1); default from rand::thread_rng
pub(super) async fn run_scheduler(core: Arc<Core>)                  // one task, ticks every tick_secs, woken by QueueChanged/settings/online
Core::schedule_tick(self: &Arc<Self>) -> Result<()>
// State: schedule: HashMap<String, (Timestamp, u32)> (item -> at, estimate at draw); last_automatic_exit: HashMap<String, Timestamp>
// Live: online_since: Option<Timestamp>, set when online turns true, cleared offline
// testing:
pub async fn real_backend_with(config, port, chain_factory, devices, clock: Clock, draw: Draw, timing: ExitTiming) -> Result<Arc<RealBackend>>
pub fn run_scheduler_now(backend: &RealBackend)
```
Per tick and connected federation: `tend_queue`; fee quotes cached per denomination for 600 s; `readiness` per item; a `Ready` item without a draw, or whose estimate changed, draws `now + draw_delay(mean_wait_secs(estimate, target), draw())`; a due item exits through `start_exit(.., early: false)` unless `not_before` says later, in which case it is redrawn from that time. At most one automatic exit per federation per tick. Reconnect and settings changes clear that federation's draws; nothing is persisted, so a restart redraws. `QueueState` shows `Scheduled{at}`.

- [ ] **Step 1: Failing tests (injected clock and draw):** `an_eligible_item_exits_at_its_drawn_time` (u = 0.5, estimate 50: none at +7_485 s, one at +7_486 s), `restart_redraws_the_delay`, `a_new_estimate_redraws_the_delay`, `a_settings_change_redraws_the_schedule`, `no_exit_within_the_grace_period_after_start_or_reconnect` (Review Focus 3: flap online every 300 s for an hour, no withdraw call, status `GracePeriod`), `fee_guard_defers_automatic_exits`, `exit_waits_when_spending_cannot_pay_the_fee`, `two_items_never_exit_within_an_hour`, `an_automatic_exit_is_not_flagged_early`, `offline_pauses_the_scheduler`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Devimint test,** both generations: `an_automatic_exit_lands` (`ExitTiming::FAST`, settings min time 0, grace 0, anonymity target 1; a 0.01 item lands within 300 s tagged `Anonymity{estimate}`).
- [ ] **Step 4:** Gates. Commit `feat(core): exit at Poisson-drawn times`.

### Task 8: Demo backend [parallel with Tasks 3-7, after Task 2]

**Files:** Modify `crates/fedimix-core/src/demo/{mod,fixtures}.rs`. Create `scripts/demo-flows/30-exits.drive`.

Demo accounting per O1: a Spending send's debit above for-spending shrinks the queue newest first (`scheduler::fit`) and the preview carries the same note as Task 5; `exit_now` re-checks the quoted fee against for-spending. New scenario `exits`: on each `DemoTiming` step the first item goes `Scheduled` → `Withdrawing { txid: None }` → `Withdrawing { txid: Some }` → landed (leaves the queue), Saving `in_flight` shows it meanwhile, the coin is tagged `Anonymity{estimate}` (manual exits stay `EarlyExit`), and `Notice::ExitLanded` fires.

- [ ] **Step 1: Failing tests:** `a_demo_send_shrinks_the_queue_newest_first`, `demo_exit_now_rechecks_the_fee`, `the_exits_scenario_lands_an_unflagged_coin`, existing `queue_total_equals_moving_all_scenarios` covers `exits`.
- [ ] **Step 2: Run; expect FAIL.** Implement. Flow 30: `--demo-scenario exits`, Moving to Saving page, a row reaching "withdrawing", Saving's "+0.01 depositing ›", the coin's "anon set N" tag; shots at each.
- [ ] **Step 3:** Gates including `just interact-demo` three times. Commit `feat(demo): exit the queue on its own and shrink it on sends`.

### Task 9: GTK and real UI

**Files:** Modify `crates/fedimix-core/src/real/spending.rs` (`features.scheduler = true`), `crates/fedimix-gtk/src/{present,cli,main}.rs`, `ui/{moving_page,settings_page,saving_tab,spending_tab,send_dialog,coin_detail}.rs`, `scripts/test-real-ui.sh`, `scripts/real-flows/saving-refill.drive` (it asserts the Exit group is absent). Create `scripts/real-flows/exits.drive`.

Real mode now shows the Exit settings group, the Spending caption's "moving to Saving ›" link, the Moving to Saving page, and Saving's "+X depositing ›" line; Cover stays hidden. The "Exit now?" body names the current estimate: "Leaving early lowers your privacy: this note leaves with an anonymity set of about N. Withdrawal fee: F." Settings' federation rows show "anon ~N", and "Most private" on the largest. `--exit-timing fast` exists only with `fedimix-gtk/testing`, as `--test-signer` does, and passes `ExitTiming::FAST`.

- [ ] **Step 1: Failing tests:** `present::exit_warning_names_the_estimate`, `present::moving_links_with_a_scheduler` (replaces `moving_reads_as_waiting_without_a_scheduler`), `present::most_private_federation`, `cli::exit_timing_needs_the_feature`.
- [ ] **Step 2: Run; expect FAIL.** Implement.
- [ ] **Step 3: Real flow** `exits.drive` (software Saving, `--exit-timing fast`, settings min hours 0, grace 0, anonymity target 1): fund Spending to target + 0.011 BTC; the page shows 0.01; Exit now on it, lands as "early exit"; a second deposit's item lands on its own as "anon set N". Read the screenshots.
- [ ] **Step 4:** Gates including `just interact-demo`. Commit `feat(gtk): show and run exits on the real backend`.

### Task 10: Recorded exit demo

**Files:** Create `scripts/record-exits.sh`, `scripts/real-flows/exits-demo.drive`. Modify `justfile` (`record-exits *args`), `flake.nix` (`wf-recorder` in the integration shell).

`just record-exits` starts devimint (v2), the real binary under headless sway at 1280×800 with `--exit-timing fast` and the software Saving wallet, records with `wf-recorder` to `target/recordings/exits.mp4`, and drives `exits-demo.drive`: join, deposit, the queue and its meters, an early exit with its fee, an automatic exit landing, the Coins page tags. Each step holds 2-3 s (`wait`) so the video reads. `just record-exits --demo` records flow 30 on the demo backend instead, with no devimint.

- [ ] **Step 1:** Write the script and drive; run `just record-exits` and `just record-exits --demo`; expect each mp4 above 1 MB and `DRIVE OK`. Watch both start to end.
- [ ] **Step 2:** Gates. Commit `chore: record a demo of exits`.

### Task 11: Close out

- [ ] **Step 1:** README: how exits work (target, queue, anonymity estimate and what it counts and leaves out, Poisson wait and its numbers, grace, fee guard, early exit, labels), `just record-exits`. status.md: Phase 4 done, what works with commands, "Not verified" (mainnet session sizes and sync time, LNv1 federations, timing under real fees), "Needs user" (the wait formula and spacing values; whether the active federation should follow the largest estimate; whether to count LNv1 receives).
- [ ] **Step 2:** `nix build` and `just build-nix` pass. Commit `docs: describe exits`.
