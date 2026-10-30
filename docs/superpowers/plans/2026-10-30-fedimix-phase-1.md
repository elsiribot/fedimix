# Fedimix Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the scaffold into `fedimix-core` + `fedimix-gtk`, define the FFI-friendly core API, and build the complete GTK shell running against a `--demo` fake backend that matches the mockups.

**Architecture:** `fedimix-core` (no GTK) exposes a `Fedimix` handle over a `Backend` trait, snapshot types and an `Event` stream; Phase 1 ships only `DemoBackend`. `fedimix-gtk` runs the core on a tokio runtime in its own thread, calls it through `CoreBridge`, and on every state event refetches snapshots and calls `update()` on each view. Pure display logic lives in `format.rs`/`present.rs` so it is unit-testable without a display.

**Tech Stack:** Rust 2024, gtk4 0.11, libadwaita 0.9 (feature `v1_6`), tokio, async-channel, async-trait, thiserror, qrcode. Nix flake + just.

**Spec:** `docs/superpowers/specs/2026-10-30-fedimix-design.md`. Mockups (gitignored, read-only): `.superpowers/brainstorm/158472-1793384045/content/ui-v7.html`, `pages-v6.html`. In `pages-v6.html` the words "hot wallet"/"cold wallet" are superseded by the spec's "Spending"/"Saving".

## Global Constraints

- `fedimix-core` must not depend on GTK/glib. Public core API uses owned, FFI-friendly types: no generics or borrowed lifetimes in public signatures.
- Every command runs inside the flake: `nix develop -c just clippy`, `nix develop -c just test`.
- Before every commit both must pass. Commit subjects: `feat:`, `fix:`, `docs:`, `chore:`, `test:`; end messages with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Stay on `master`. No branches or worktrees. Never run `git worktree prune` or `git gc`. Never commit `ANALYSIS.md` or `.superpowers/`.
- UI built with plain Rust builders (as the existing `window.rs`), no Blueprint/XML. Each view: a struct with `widget()` and `update(&...)`.
- No `unwrap()`/`expect()` on data from the backend or the user. A failed core call becomes an `adw::Toast`, never a panic.
- Principle: show what most users need; detail lives behind an amount with a `›`.
- Exact defaults: Spending target 0.05 BTC; anonymity target 50; minimum time in pool 24 h; online grace 10 min; fee guard 2%; cover budget 20,000 sat/month.
- Exact copy: "Saving", "Spending", "Show cover traffic", "Moving to Saving", "Exit now (lower privacy)", "Connect a hardware wallet", "Refill from Saving", "Set up Saving to move X BTC to cold storage", "Set up ›", "waiting for Saving", "From Spending", "change → Spending", "unmixed", "early exit", "small change", "Offline".

## Review Focus

1. Long federation names, invoices and addresses at 360 px width: labels wrap/ellipsize, no horizontal overflow (Task 6, 7, 9; the screenshot run in Task 12 includes a narrow pass).
2. Invalid Send input (garbage recipient, empty or zero amount, amount above balance): inline error, no crash, Confirm disabled (Task 9 present tests; Task 3 demo tests).
3. Amounts smaller than display precision must never read as `0.0000 BTC` (Task 4 format tests).
4. Double-activating Confirm must send once; a core error must surface inline in Send and as a toast in Refill (Task 9, 10).
5. State changing under an open page: a queue item exiting while Moving to Saving is open, or "Exit now" on an item that already left, gives a refreshed list and a toast (Task 3 test `exit_now_unknown_item_errors`, Task 8).

---

### Task 1: Workspace split, dev shell and screenshot harness

**Files:**
- Modify: `Cargo.toml` (becomes workspace root with `[workspace.dependencies]`), `flake.nix`, `justfile`, `README.md`
- Create: `crates/fedimix-core/{Cargo.toml,src/lib.rs}`, `crates/fedimix-gtk/Cargo.toml`, `scripts/screenshot.sh`, `docs/superpowers/status.md`, `docs/superpowers/decisions.md`
- Move: `src/main.rs`, `src/window.rs` → `crates/fedimix-gtk/src/`

**Interfaces:**
- Produces: `just test` (`cargo test --workspace`), `just clippy` (add `--workspace`), `just run` (`cargo run -p fedimix-gtk`), `just demo` (`cargo run -p fedimix-gtk -- --demo`), `just screenshot <name> [args...]`.
- `scripts/screenshot.sh <out.png> [--width W] [--height H] -- <fedimix args>`.

- [ ] **Step 1:** Create the workspace. Binary name stays `fedimix` (`[[bin]] name = "fedimix"`, package `fedimix-gtk`). Move gtk/adw deps to `[workspace.dependencies]`, set adw feature `v1_6`. Update `Cargo.lock`. Crane `meta.mainProgram` stays valid; add `cargoExtraArgs = "-p fedimix-gtk"` only if `nix build` needs it.
- [ ] **Step 2:** Add to `devShells.default.packages`: `pkgs.sway pkgs.grim pkgs.jq pkgs.dbus`. Add `LD_LIBRARY_PATH = lib.makeLibraryPath buildInputs` (without it the built binary fails with `libadwaita-1.so.0: cannot open shared object file`).
- [ ] **Step 3:** Write `scripts/screenshot.sh`. Verified recipe: a short runtime dir (`mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}"`; a long path breaks the Wayland socket), `WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 sway -c <conf> --unsupported-gpu`, where `<conf>` sets the output resolution and a solid `bg`. Run the app with `GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none` (the ambient environment sets `GDK_BACKEND=x11`, which silently sends the window elsewhere). Use `WAYLAND_DISPLAY` and `SWAYSOCK` discovered from the runtime dir, wait until `swaymsg -t get_tree` shows `app_id` (max 15 s) plus 1 s, `grim <out>`, then kill both by PID and remove the runtime dir via `trap`. Non-zero exit if the window never maps. Default size 1280x720; `--width/--height` set the sway output size.
- [ ] **Step 4:** Add `docs/superpowers/status.md` (≤30 lines: phase/task, what works with commands, next, "Needs user") and `docs/superpowers/decisions.md` (bullets).
- [ ] **Step 5:** Verify: `nix develop -c just clippy`, `nix develop -c just test`, `nix develop -c just build`, and `nix develop -c scripts/screenshot.sh target/screenshots/scaffold.png` writes a PNG showing the placeholder window. Keep `.gitignore`d output under `target/`.
- [ ] **Step 6:** Commit `chore: split into fedimix-core and fedimix-gtk workspace`.

### Task 2: Core API types and `Fedimix` handle

**Files:**
- Create: `crates/fedimix-core/src/{api.rs,event.rs,error.rs,handle.rs,backend.rs}`; modify `lib.rs` (re-export all)

**Interfaces — Produces (exact):**

```rust
// api.rs — all types: Debug, Clone, PartialEq; plain owned data.
#[derive(Copy, Eq, Ord, PartialOrd, Default)]
pub struct Amount { pub msat: u64 }
impl Amount { pub const ZERO: Amount; pub fn from_sat(u64)->Self; pub fn as_sat(self)->u64; pub fn checked_add/checked_sub/saturating_sub }
pub type Timestamp = u64;           // unix seconds
pub enum WalletKind { Saving, Spending }

pub struct DeviceInfo { pub name: String, pub connected: bool }
pub struct FederationInfo { pub id: String, pub name: String, pub online: bool, pub anonymity_estimate: u32 }

pub enum HistoryKind {
    FromSpending { denomination: Amount }, Sent { change: Option<SentChange> }, Received { unmixed: bool },
    Paid, ReceivedLightning, ToSaving { denomination: Amount }, Deposit, Cover,
}
pub struct SentChange { pub amount: Amount, pub to_spending: bool }  // to_spending=false => "small change"
pub enum HistoryStatus { Confirming { confirmations: u32, required: u32 }, Complete }
pub struct HistoryItem { pub id: String, pub kind: HistoryKind, pub amount: Amount, pub incoming: bool,
                         pub status: HistoryStatus, pub timestamp: Timestamp }

pub enum PrivacyTag { Anonymity { estimate: u32 }, EarlyExit { estimate: u32 }, Unmixed, SmallChange }
pub struct Coin { pub outpoint: String, pub amount: Amount, pub tag: PrivacyTag, pub received_at: Timestamp,
                  pub address: String, pub source_federation: Option<String>, pub frozen: bool }

pub struct SavingState { pub device: Option<DeviceInfo>, pub balance: Amount, pub in_flight: Amount,
                         pub coins: Vec<Coin>, pub history: Vec<HistoryItem> }
pub struct NoteBucket { pub denomination: Amount, pub count: u32 }
pub struct SpendingState { pub federation: Option<FederationInfo>, pub balance: Amount, pub target: Amount,
                           pub for_spending: Amount, pub moving: Amount, pub saving_ready: bool,
                           pub notes: Vec<NoteBucket>, pub history: Vec<HistoryItem> }
pub enum QueueItemStatus { Queued, Eligible, Scheduled { at: Timestamp }, Withdrawing { txid: String }, Landed }
pub struct QueueItem { pub id: String, pub denomination: Amount, pub status: QueueItemStatus,
                       pub anonymity_current: u32, pub anonymity_attainable: u32 }
pub struct QueueState { pub total: Amount, pub anonymity_estimate: u32, pub typical_exit_secs: u64,
                        pub waiting_for_saving: bool, pub items: Vec<QueueItem> }

pub struct SendRequest { pub wallet: WalletKind, pub destination: String, pub amount: Option<Amount> }
pub struct SendInput { pub outpoint_or_note: String, pub amount: Amount, pub tag: Option<PrivacyTag> }
pub enum ChangeDestination { None, Spending { amount: Amount }, SmallChange { amount: Amount } }
pub struct SendPreview { pub id: String, pub wallet: WalletKind, pub destination: String, pub amount: Amount,
                         pub fee: Amount, pub inputs: Vec<SendInput>, pub change: ChangeDestination }
pub struct RefillPreview { pub id: String, pub amount: Amount, pub fee: Amount, pub coins: Vec<Coin>,
                           pub overshoot: Amount, pub device_name: String }
pub enum ReceiveMethod { Lightning { amount: Amount }, OnChain }
pub struct ReceiveRequest { pub wallet: WalletKind, pub method: ReceiveMethod }
pub struct ReceiveInfo { pub payload: String, pub uri: String }   // payload = text shown; uri = QR content
pub struct SuggestedFederation { pub name: String, pub invite: String }
pub enum ChainBackend { Esplora { url: String }, Electrum { url: String } }
pub struct Settings { pub spending_target: Amount, pub anonymity_target: u32, pub min_time_in_pool_secs: u64,
                      pub online_grace_secs: u64, pub fee_guard_percent: u32, pub cover_budget_sat_per_month: u64,
                      pub chain_backend: ChainBackend, pub socks5_proxy: Option<String> }
impl Default for Settings   // spec defaults; Esplora url "https://blockstream.info/api"

// event.rs
pub enum Notice { DepositConfirmed { amount: Amount }, ExitLanded { amount: Amount },
                  SendConfirmed { amount: Amount }, SavingSetupNudge { amount: Amount } }
pub enum Event { BalanceChanged { wallet: WalletKind, balance: Amount },
                 HistoryItemAdded { wallet: WalletKind, item: HistoryItem },
                 HistoryItemUpdated { wallet: WalletKind, item: HistoryItem },
                 QueueChanged, ConnectionChanged { online: bool }, FederationChanged, SavingChanged,
                 SettingsChanged, Notification(Notice) }
pub struct EventStream { /* wraps async_channel::Receiver<Event> */ }
impl EventStream { pub async fn next(&self) -> Option<Event> }

// error.rs — thiserror, Clone, PartialEq; String payloads only
pub enum FedimixError { InvalidDestination(String), InvalidAmount(String), InsufficientFunds { available: Amount, needed: Amount },
                        InvalidInvite(String), UnknownItem(String), DeviceNotConnected(String), Offline, NotSetUp(String), Internal(String) }
pub type Result<T> = std::result::Result<T, FedimixError>;

// backend.rs — #[async_trait]
pub trait Backend: Send + Sync + 'static {
    fn subscribe(&self) -> EventStream;
    async fn saving_state(&self) -> Result<SavingState>;
    async fn spending_state(&self) -> Result<SpendingState>;
    async fn queue_state(&self) -> Result<QueueState>;
    async fn settings(&self) -> Result<Settings>;
    async fn set_settings(&self, settings: Settings) -> Result<()>;
    async fn suggested_federations(&self) -> Result<Vec<SuggestedFederation>>;
    async fn join_federation(&self, invite: String) -> Result<FederationInfo>;
    async fn federations(&self) -> Result<Vec<FederationInfo>>;
    async fn set_active_federation(&self, id: String) -> Result<()>;
    async fn detect_devices(&self) -> Result<Vec<DeviceInfo>>;
    async fn setup_saving(&self, device: DeviceInfo) -> Result<()>;
    async fn receive(&self, request: ReceiveRequest) -> Result<ReceiveInfo>;
    async fn prepare_send(&self, request: SendRequest) -> Result<SendPreview>;
    async fn confirm_send(&self, preview_id: String) -> Result<()>;
    async fn prepare_refill(&self) -> Result<RefillPreview>;
    async fn confirm_refill(&self, preview_id: String) -> Result<()>;
    async fn exit_now(&self, item_id: String) -> Result<()>;
    async fn set_coin_frozen(&self, outpoint: String, frozen: bool) -> Result<()>;
}

// handle.rs
#[derive(Clone)] pub struct Fedimix { /* Arc<dyn Backend> */ }
impl Fedimix { pub fn new(backend: Arc<dyn Backend>) -> Self; /* one forwarding pub method per Backend method, same names */ }
```

BTC string parsing is not in core; Task 4 owns it in `format::parse_btc`.

- [ ] **Step 1: Write failing tests** in `api.rs`/`event.rs`:
  `amount_sat_roundtrip` (`Amount::from_sat(5).msat == 5000`, `.as_sat() == 5`), `amount_max_supply_fits` (`Amount::from_sat(2_100_000_000_000_000)` no overflow), `settings_defaults_match_spec` (target 5_000_000 sat, anonymity 50, 86_400 s, 600 s, 2 %, 20_000 sat, Esplora, no proxy), `event_stream_ends_when_sender_dropped`.
- [ ] **Step 2:** Run `nix develop -c cargo test -p fedimix-core`; expect compile failure.
- [ ] **Step 3:** Implement. `EventStream` has `pub fn new(rx: async_channel::Receiver<Event>)`.
- [ ] **Step 4:** Tests pass, clippy clean.
- [ ] **Step 5:** Commit `feat: add core API types, events and Fedimix handle`.

### Task 3: Demo backend with scenarios

**Files:**
- Create: `crates/fedimix-core/src/demo/{mod.rs,fixtures.rs}`, test module inside `mod.rs`

**Interfaces:**
- Consumes: Task 2.
- Produces: `pub enum DemoScenario { Funded, BelowTarget, NoSaving, FirstRun, Offline }`, `impl FromStr for DemoScenario` (`funded`, `below-target`, `no-saving`, `first-run`, `offline`), `pub struct DemoBackend`, `DemoBackend::new(scenario: DemoScenario) -> Arc<Self>`.

Fixtures (BTC; match the mockups exactly; `now` = fixed base timestamp `1_793_318_400` for determinism):

| Scenario | Saving | Spending | Queue |
|---|---|---|---|
| Funded | device "Ledger Nano S+" connected; balance 1.2034, in_flight 0.42; coins 0.1 (est 64), 0.02 (71), 0.1 (52), 0.02 (EarlyExit 12), 0.01 (88), 0.9534 (Unmixed) | "Fedi Alpha", online, balance 0.47, for_spending 0.05, moving 0.42, target 0.05 | 0.2 (35→60), 0.1 (35→60), 0.1 (12→48), 0.02 (71→71, Eligible); estimate 60; typical 86_400 s |
| BelowTarget | as Funded device; coins 0.01 (88), 0.05 (EarlyExit 9), 0.1 (64), 0.9534 (Unmixed); balance 1.1134, in_flight 0 | 0.0112, for_spending 0.0112, moving 0 | empty |
| NoSaving | device None, balance 0 | as Funded but `saving_ready=false` | same 4 items, `waiting_for_saving=true` |
| FirstRun | device None | federation None, balance 0 | empty |
| Offline | as Funded | federation `online=false` | as Funded |

History: Saving and Spending rows from the mockup, same order and values (`Sent` 0.25 with `change` 0.0313 to Spending; `Deposit` 0.05 `Confirming{1,3}`). Funded Spending history also has two `Cover` items hidden by default. Notes: buckets summing to the balance.

Behaviour:
- `prepare_send`: destination valid if it starts with `lnbc`, `bc1`, `tb1` or `bcrt1`, else `InvalidDestination`; amount required for on-chain, `None` on `lnbc` means fixed-amount invoice of 0.0012; amount zero → `InvalidAmount`; amount+fee > balance → `InsufficientFunds`. Fee: 420 sat on-chain, 12 sat Lightning. Saving sends return `ChangeDestination::Spending`, or `SmallChange` when change < 10,000 sat. `offline` scenario returns `Offline` for Spending sends.
- `confirm_send`: unknown id → `UnknownItem`; consuming an id twice → `UnknownItem`. Mutates balance, adds history, emits `BalanceChanged`, `HistoryItemAdded`, and later (tokio sleep 2 s) `Notification(SendConfirmed)`.
- `prepare_refill` / `confirm_refill`: smallest-first until covering the gap, no change. In BelowTarget: amount 0.06, fee 420 sat, overshoot 0.0212, coins 0.01 + 0.05. At or above target → `InvalidAmount("Spending is at target")`. No device → `NotSetUp`.
- `exit_now`: unknown id → `UnknownItem`; removes the item, adds `ToSaving` and `FromSpending` history, a coin tagged `EarlyExit`, emits events and `Notification(ExitLanded)`.
- `join_federation`: invite must start with `fed1`, else `InvalidInvite`; success sets federation, spending usable. `setup_saving` sets device, `saving_ready=true`, `waiting_for_saving=false`. `set_settings` stores and emits. `receive` returns deterministic strings (`lnbc...`, `bc1q...`) with `bitcoin:`/`lightning:` URI.
- All mutable state in one `tokio::sync::Mutex`; lock never held across an `.await` of a sleep.

- [ ] **Step 1: Write failing tests** (tokio test): `funded_matches_mockup` (balances), `coins_sum_to_balance_all_scenarios`, `queue_total_equals_moving_all_scenarios`, `spending_parts_sum_to_balance_all_scenarios`, `refill_below_target_matches_mockup`, `refill_at_target_rejected`, `exit_now_moves_item_and_emits`, `exit_now_unknown_item_errors`, `send_rejects_garbage_destination`, `send_rejects_zero_amount`, `send_rejects_above_balance`, `confirm_send_twice_errors`, `join_rejects_bad_invite`, `first_run_join_then_setup_saving`, `scenario_from_str_rejects_unknown`.
- [ ] **Step 2:** Run; compile failure.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Tests + clippy pass.
- [ ] **Step 5:** Commit `feat: add demo backend with mockup scenarios`.

### Task 4: Pure formatting and presentation logic

**Files:**
- Create: `crates/fedimix-gtk/src/format.rs`, `crates/fedimix-gtk/src/present.rs`

**Interfaces — Produces:**

```rust
// format.rs
pub fn btc(a: Amount) -> String             // "1.2034 BTC", 4 dp truncated; non-zero below 0.0001 -> "<0.0001 BTC"; zero -> "0 BTC"
pub fn btc_plain(a: Amount) -> String       // same without unit
pub fn btc_short(a: Amount) -> String       // trailing zeros trimmed: "0.05", "0.42", "1", "0.0112"
pub fn btc_exact(a: Amount) -> String       // "0.00012345 BTC", 8 dp
pub fn signed(a: Amount, incoming: bool) -> String  // "+0.1000" / "−0.2500" (U+2212)
pub fn sats(a: Amount) -> String            // "420 sat"
pub fn parse_btc(s: &str) -> Result<Amount, String>  // max 8 dp, trims; rejects negative, empty, overflow above 21,000,000
pub fn date(ts: Timestamp, now: Timestamp) -> String // same local day "10:42", else "Oct 29"
pub fn duration_rough(secs: u64) -> String  // "~1 day", "~3 hours", "~20 min"

// present.rs
pub fn spending_caption(s: &SpendingState) -> Caption  // { text, opens_queue: bool, shows_refill: bool, fraction: f64 }
pub fn saving_banner(s: &SpendingState) -> Option<String> // Some("Set up Saving to move 0.42 BTC to cold storage")
pub fn in_flight_line(s: &SavingState) -> Option<String>  // Some("+0.4200 BTC depositing ›"), None when zero
pub fn privacy_tag(t: &PrivacyTag) -> TagView          // { text, css_class }
pub fn history_row(item: &HistoryItem, now: Timestamp) -> HistoryRowView // { icon, title, tag, amount, trailing, nested }
pub fn queue_row(item: &QueueItem) -> QueueRowView     // { label, fraction, detail }: "35 → ~60", "71 · eligible"
pub fn queue_summary(q: &QueueState) -> String         // "Est. anonymity ~60 · typical exit within ~1 day"
pub fn visible_history(items: &[HistoryItem], show_cover: bool) -> Vec<HistoryItem>
pub fn send_validation(dest: &str, amount: &str, requires_amount: bool) -> Result<(), String>
```

CSS classes from `privacy_tag`: `tag-mixed`, `tag-early`, `tag-unmixed`, `tag-change`, `tag-cover`. Meter fraction = current / anonymity target, clamped to 1.

- [ ] **Step 1: Write failing tests:** `btc_formats_mockup_values` (120_340_000 sat → "1.2034 BTC"), `btc_never_shows_zero_for_dust` (1 sat → "<0.0001 BTC"; 0 → "0 BTC"), `btc_short_trims` ("0.05", "0.42"), `signed_uses_true_minus`, `parse_btc_rejects_bad_input` ("", "abc", "-1", "0.123456789", "21000001"), `parse_btc_accepts` ("0.0012" → 120_000 sat), `caption_above_target` ("0.05 for spending · 0.42 moving to Saving ›", fraction ≈ 0.106), `caption_below_target` ("0.0112 of 0.05 target", shows_refill, fraction ≈ 0.224), `caption_waiting_for_saving` ("0.05 for spending · 0.42 waiting for Saving", opens_queue false), `banner_only_when_no_saving_and_moving`, `in_flight_hidden_when_zero`, `queue_row_attainable` ("35 → ~60"), `queue_row_eligible` ("71 · eligible"), `cover_hidden_unless_checked`, `history_sent_has_nested_change` ("change → Spending"), `small_change_nested_label`, `validation_inline_errors`, `caption_zero_target_no_nan`.
- [ ] **Step 2:** Run `nix develop -c cargo test -p fedimix-gtk`; compile failure.
- [ ] **Step 3:** Implement. Add `fedimix-core` as a dependency; declare modules in `main.rs` with `#[allow(dead_code)]` removed as Task 5+ use them (a temporary crate-level allow is acceptable until Task 12 removes it).
- [ ] **Step 4:** Tests + clippy pass.
- [ ] **Step 5:** Commit `feat: add display formatting and presentation logic`.

### Task 5: App shell, core bridge and window

**Files:**
- Create: `crates/fedimix-gtk/src/{cli.rs,bridge.rs,style.css,ui/mod.rs,ui/app_state.rs}`; modify `main.rs`, `window.rs`

**Interfaces — Produces:**

```rust
// cli.rs
pub struct Args { pub demo: bool, pub scenario: DemoScenario, pub page: Option<StartPage> }
pub enum StartPage { Saving, Spending, Coins, CoinDetail, Moving, Notes, SendReview, Send, Receive, Refill, RefillExpanded,
                     Join, SetupSaving, Settings, About }
pub fn parse(args: impl Iterator<Item = String>) -> Result<Args, String>
// flags: --demo, --demo-scenario <name>, --demo-page <name> (kebab-case). --demo-* without --demo is an error.

// bridge.rs
#[derive(Clone)] pub struct CoreBridge { /* Fedimix + tokio Handle */ }
impl CoreBridge {
    pub fn start(backend: Arc<dyn Backend>) -> Self;  // runtime on its own named thread "fedimix-core"
    pub fn core(&self) -> Fedimix;
    // spawn on the runtime; deliver the result on the GTK main loop
    pub fn call<T, Fut, Cb>(&self, fut: Fut, on_done: Cb) where T: Send + 'static, Fut: Future<Output = T> + Send + 'static, Cb: FnOnce(T) + 'static;
    pub fn events(&self) -> async_channel::Receiver<Event>;  // forwarded from EventStream
}

// ui/app_state.rs — refetches all three snapshots on any state event and fans out
pub struct Snapshots { pub saving: SavingState, pub spending: SpendingState, pub queue: QueueState }
pub struct AppContext { pub bridge: CoreBridge, pub toasts: adw::ToastOverlay, pub nav: adw::NavigationView, pub stack: adw::ViewStack }
impl AppContext {
    pub fn toast(&self, message: &str);
    pub fn on_snapshots(&self, f: impl Fn(&Snapshots) + 'static); // registered views; also called once with the latest
    pub fn latest(&self) -> Option<Snapshots>;
    pub fn refresh(&self);
    pub fn run<T>(&self, fut, on_ok: impl FnOnce(T) + 'static)  // error → toast
}
```

Behaviour:
- `main` parses args **before** `app.run_with_args::<&str>(&[])` so GApplication does not reject custom flags. Bad args print to stderr and exit 2. Without `--demo` print `Fedimix currently only runs with --demo` and exit 2 (Phase 2 replaces this).
- Window layout: `adw::ToastOverlay` > `adw::NavigationView` > root `NavigationPage` > `adw::ToolbarView`: top `HeaderBar` with `adw::ViewSwitcher` (policy Wide) as title, ⋮ `MenuButton` right (Settings, About, Quit); bottom `adw::ViewSwitcherBar` shown via `adw::Breakpoint` at `max-width: 500sp` where the header switcher is hidden. Min size 360×480, default 480×640. Saving is the default tab; `Spending` is default when no Saving wallet is set up (the mockups put Saving first; order stays Saving, Spending).
- Load `style.css` via `gtk::CssProvider`: `.hero-balance` (font-size 200%, bold), tag classes with colours from the mockups, using `@success_*`/`@warning_*`/`@accent_*`/`@error_*` colours so dark mode works.
- `--demo-page` is only parsed here; Task 7 starts applying it.
- Tabs are placeholder `adw::StatusPage`s until Tasks 6–7 replace them.

- [ ] **Step 1: Write failing tests** in `cli.rs`: `parses_demo`, `parses_scenario_and_page`, `rejects_unknown_flag`, `rejects_scenario_without_demo`, `no_args_is_not_demo`. In `bridge.rs`: `call_delivers_on_main_context`, which iterates a `glib::MainContext` until the callback fires (no display required).
- [ ] **Step 2:** Run; compile failure.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Tests + clippy pass; `nix develop -c scripts/screenshot.sh target/screenshots/shell.png -- --demo` shows switcher; again with `--width 400` shows the bottom bar. Read both PNGs.
- [ ] **Step 5:** Commit `feat: add application shell, core bridge and demo flag`.

### Task 6: Saving tab

**Files:** Create `crates/fedimix-gtk/src/ui/{saving_tab.rs,history_list.rs,widgets.rs}`; modify `window.rs`

**Interfaces — Produces:**
- `widgets::amount_button(css: &str) -> AmountButton` — flat button with hero amount label + `›`; `set_amount(&str)`, `connect_clicked`.
- `widgets::hero_box()`, `widgets::send_receive_row(&AppContext, WalletKind)`: centered pill `Send` (`suggested-action`) and `Receive`, with signals; Task 9 fills in the dialog openers via `ui::send_dialog::open(ctx, wallet)` and `ui::receive_dialog::open(ctx, wallet)` — until then they show a toast `"Not available yet"`.
- `history_list::HistoryList::new()`, `.widget()`, `.set_items(&[HistoryRowView])` — `gtk::ListBox` `boxed-list`, nested change rows drawn indented without own card; empty state row "No activity yet".
- `saving_tab::SavingTab::new(ctx: Rc<AppContext>)`, `.widget()`.

Layout (per mockup), in a `gtk::ScrolledWindow` + `adw::Clamp` (max 600): device status line (`dim-label`, USB-symbolic icon), balance `AmountButton` → pushes Coins (Task 7 provides `ui::coins_page::push(ctx)`; until then toast), `in_flight_line` link button → `ctx.stack.set_visible_child_name("spending")`, Send / Receive, heading "History", list. When `device` is `None`: `adw::StatusPage` title "Connect a hardware wallet", description "Saving is your cold storage. Spending works without it.", button "Set up" → `ui::setup_saving::open(ctx)` (Task 11; toast until then).

- [ ] **Step 1:** No display tests here; row content is covered by Task 4. Implement. Federation/device names use `ellipsize=End`.
- [ ] **Step 2:** Clippy + tests pass.
- [ ] **Step 3:** Screenshot `-- --demo` at 480×640 and at 360×640, and `--demo --demo-scenario no-saving`. Read the PNGs and compare to the Saving card in `ui-v7.html`: all five history rows, the nested change row, `+0.4200 BTC depositing ›`.
- [ ] **Step 4:** Commit `feat: add Saving tab`.

### Task 7: Spending tab, Coins and notes pages

**Files:** Create `crates/fedimix-gtk/src/ui/{spending_tab.rs,coins_page.rs,coin_detail.rs,notes_page.rs}`; modify `window.rs`, `saving_tab.rs`

**Interfaces — Produces:** `SpendingTab::new(ctx)`, `coins_page::push(&Rc<AppContext>)`, `coin_detail::push(&Rc<AppContext>, Coin)`, `notes_page::push(&Rc<AppContext>)`; consumes `moving_page::push` (Task 8; toast until then) and `refill_dialog::open` (Task 10; toast until then).

Spending layout: Offline `adw::Banner` ("Offline") when federation not online; nudge `adw::Banner` from `present::saving_banner` with button "Set up ›"; federation name; balance `AmountButton` → notes page; `gtk::LevelBar`-style bar with `Caption.fraction` in a 200 px clamp; caption label (link-styled and clickable when `opens_queue`); "Refill from Saving" link button when `shows_refill`; Send/Receive; history heading row with "History" left and a dim right-aligned `gtk::CheckButton` "Show cover traffic" (unchecked); list via `visible_history`.

Coins page: `adw::NavigationPage` title "Coins", `boxed-list` rows: amount, privacy tag pill, date; row → detail. Coin detail: `adw::PreferencesGroup` of `ActionRow`s: Amount (exact), Transaction (`outpoint`, selectable, wraps), Address (selectable, wraps), Source federation, Anonymity at exit, and a `SwitchRow` "Freeze" calling `set_coin_frozen`. Notes page: title "Balance", rows `<count> × <denomination>`.

- [ ] **Step 1:** Implement.
- [ ] **Step 2:** Clippy + tests pass.
- [ ] **Step 3:** Implement `apply_start_page` in `window.rs` with the `Saving`, `Spending`, `Coins`, `CoinDetail` and `Notes` start pages; each later task adds its own pages there. Screenshot Spending in the `funded`, `below-target`, `no-saving` and `offline` scenarios. Read the PNGs and compare with `ui-v7.html` and `pages-v6.html`.
- [ ] **Step 4:** Commit `feat: add Spending tab, Coins and notes pages`.

### Task 8: Moving to Saving page

**Files:** Create `crates/fedimix-gtk/src/ui/moving_page.rs`; modify `spending_tab.rs`

**Interfaces — Produces:** `moving_page::push(&Rc<AppContext>)`.

Layout: title "Moving to Saving"; total (`btc`), `queue_summary` caption; one row per item: denomination, `gtk::LevelBar` 56 px with `fraction`, `detail`, ⋯ `MenuButton` with "Exit now (lower privacy)". In `waiting_for_saving` state, summary is "Waiting for Saving" and the row menu is insensitive. Activating "Exit now" opens an `adw::AlertDialog` (title "Exit now?", body stating lower privacy and the fee, responses Cancel / "Exit now" destructive), then `exit_now`. The page registers with `ctx.on_snapshots`, so rows rebuild live; after an `UnknownItem` error the toast "This item has already left" shows and the list refreshes. Empty queue: `adw::StatusPage` "Nothing to move".

- [ ] **Step 1:** Implement. The `UnknownItem` → toast mapping is a function `present::exit_error_message(&FedimixError) -> String`; test `exit_error_message_for_gone_item` first.
- [ ] **Step 2:** Clippy + tests pass.
- [ ] **Step 3:** Add the `Moving` start page, run `--demo --demo-page moving`, read the PNG and compare with the mockup.
- [ ] **Step 4:** Commit `feat: add Moving to Saving page`.

### Task 9: Send and Receive dialogs

**Files:** Create `crates/fedimix-gtk/src/ui/{send_dialog.rs,receive_dialog.rs,qr.rs}`; modify `widgets.rs`; add `qrcode` to the workspace

**Interfaces — Produces:** `send_dialog::open(&Rc<AppContext>, WalletKind)`, `receive_dialog::open(&Rc<AppContext>, WalletKind)`, `qr::texture(data: &str) -> Result<gdk::Texture, String>`.

Send (`adw::Dialog`, `adw::NavigationView` inside, 2 pages):
1. Recipient entry (Spending hint: "Lightning invoice or on-chain address"; Saving hint: "Bitcoin address"), amount entry (BTC), inline error label from `present::send_validation`, "Review" button; disabled until valid; pressing it calls `prepare_send`, a backend error appears inline.
2. Review: amount with `›` disclosing inputs and where change goes (`ChangeDestination` copy: "Change → Spending", "Stays in Saving as small change", or none); recipient (wraps, char-level); fee; Confirm button labelled `Confirm on <device>` for Saving, "Send" for Spending. On click the button goes insensitive with a spinner **before** the call; on error re-enable and show inline; on success close and toast.
Saving with a disconnected device: Review shows "Connect your <device>" instead of Confirm (spec section 4).

Receive (`adw::Dialog`): Spending shows `adw::ViewStack`/toggle "Lightning" (amount entry + Create invoice) and "On-chain" (deposit address); Saving shows only a direct address and the caption "Coins received this way are marked unmixed". Both show QR, selectable payload, Copy button (toast "Copied").

- [ ] **Step 1: Write failing tests:** `qr::tests::texture_size_is_nonzero_data` tested through a pure `qr::matrix(data) -> (usize, Vec<bool>)`, plus `matrix_handles_long_invoice` (2000 chars).
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** Clippy + tests pass. Add `Send`, `SendReview`, `Receive` start pages (`SendReview` opens with a demo destination `bc1qdemo...` and amount 0.25 from Saving and advances automatically). Read screenshots at 480 and 360 width.
- [ ] **Step 4:** Commit `feat: add Send and Receive dialogs`.

### Task 10: Refill dialog

**Files:** Create `crates/fedimix-gtk/src/ui/refill_dialog.rs`; modify `spending_tab.rs`

**Interfaces — Produces:** `refill_dialog::open(&Rc<AppContext>, expanded: bool)`.

`adw::AlertDialog`: heading "Refill Spending"; extra child: amount button (`btc`, `›` flips to `⌄`) over `from Saving · fee ~420 sat` and a `gtk::Revealer` with "Coins used (smallest first):", coin rows with `privacy_tag` pills, and "No change output. 0.0212 above target rejoins the exit queue." Responses `cancel` "Cancel" and `confirm` "Confirm on Ledger Nano S+" (`suggested`). `AlertDialog` closes on any response, so confirm cannot fire twice; `confirm` dispatches `confirm_refill` and a toast reports the outcome. Device disconnected: response label "Connect your <device>" disabled.

- [ ] **Step 1: Write failing test:** `present::refill_overshoot_note` returns "No change output. 0.0212 above target rejoins the exit queue."; empty string for zero overshoot.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** Clippy + tests pass. Add `Refill` and `RefillExpanded` start pages; read screenshots with `--demo-scenario below-target`. Compare with `pages-v6.html`.
- [ ] **Step 4:** Commit `feat: add Refill dialog`.

### Task 11: First run, Saving setup, settings and About

**Files:** Create `crates/fedimix-gtk/src/ui/{join_page.rs,setup_saving.rs,settings_page.rs,about.rs}`; modify `window.rs`, `spending_tab.rs`, `saving_tab.rs`

**Interfaces — Produces:** `join_page::JoinPage::new(ctx)` (root content when `spending.federation` is `None`), `setup_saving::open(&Rc<AppContext>)`, `settings_page::push(&Rc<AppContext>)`, `about::show(&Rc<AppContext>)`.

- Join: `adw::StatusPage` "Join a federation"; a suggested row with "Join", an entry "Paste an invite code" with "Join"; `InvalidInvite` shows inline. Window swaps from Join to tabs on `FederationChanged`.
- Setup: `adw::Dialog` listing `detect_devices()` with a "Set up" button per device; empty list shows "Connect your device and unlock it" + Retry. Nudge banner button and the empty-state button open it.
- Settings (`adw::PreferencesPage` pushed): groups Federations (list with active radio via `set_active_federation`, add row → Join), Spending (target), Exit (anonymity target, minimum hours, grace minutes, fee guard %), Cover traffic (budget sat), Hardware wallet (name, status), Network (Esplora/Electrum combo, URL, SOCKS5 proxy). Numeric fields are `adw::SpinRow` with these bounds: anonymity 1–1000, min hours 0–720, grace 0–120, fee guard 0–100, budget 0–10,000,000, target 0–21,000,000 BTC via entry using `format::parse_btc`. Apply on change through `set_settings`; a bad target shows the `error` class, no write.
- About: `adw::AboutDialog` (app name, version, `gtk::License::Unknown`, because the repo has no license yet).

- [ ] **Step 1: Write failing test:** `present::settings_from_form` accepts valid input and rejects a bad target with an error string.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** Clippy + tests pass; add start pages `Join`, `SetupSaving`, `Settings`, `About`; read screenshots including `first-run`.
- [ ] **Step 4:** Commit `feat: add first-run flow, Saving setup, settings and About`.

### Task 12: Screenshot walkthrough and interaction check

**Files:** Create `scripts/screenshot-all.sh`; modify `justfile` (`screenshot-all`), `README.md`, `crates/fedimix-gtk/src/main.rs`

- [ ] **Step 1:** `scripts/screenshot-all.sh` iterates every scenario × page combination that makes sense (at least all `StartPage` values) and writes `target/screenshots/<scenario>-<page>[-narrow].png`, at 480×640 and 360×640. Fails on any non-mapped window or a `CRITICAL`/`panicked` line in the app stderr.
- [ ] **Step 2:** Add `crates/fedimix-core/tests/demo_flow.rs` plus unit test `present::tests::demo_flow_presentation` (a `src/` test because the binary crate has no library target). Both drive `Fedimix` over `DemoBackend` through the same call sequence the UI uses (join → setup → send → refill → exit) and asserts the presentation functions on each snapshot.
- [ ] **Step 3:** Remove any temporary `allow(dead_code)`. Update README (demo usage, scenarios, screenshot script). Update `status.md`.
- [ ] **Step 4:** Run `just screenshot-all`, read a representative set of PNGs (one per page), fix mismatches against the mockups.
- [ ] **Step 5:** Commit `test: add demo walkthrough and screenshot harness`.
