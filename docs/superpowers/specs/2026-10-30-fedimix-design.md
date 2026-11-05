# Fedimix design

Date: 2026-10-30
Status: approved in brainstorming, pending written-spec review

Fedimix is a Linux-first GTK4/libadwaita desktop app (Android, then possibly iOS, later) that combines a Fedimint ecash wallet with a hardware-wallet-backed on-chain wallet. Dirty coins are deposited into a federation, spent from there or moved out in common denominations to cold storage once they have gained anonymity. Change from cold spends flows back into the federation automatically.

UI mockups from the brainstorming session live in `.superpowers/brainstorm/` (not committed). The final consolidated mockup is `ui-v7.html`; the refill dialog is `pages-v6.html`.

## 1. Funds model

### Wallets

- **Spending**: Fedimint ecash in one active federation. Pays and receives via Lightning and on-chain (federation deposit/withdraw).
  - Multiple federations can be joined. The default for new deposits is the one with the largest anonymity estimate. Switching is in settings, not on the main screens.
- **Saving**: on-chain watch-only wallet built from a hardware wallet xpub. Only the hardware wallet signs; the app never holds cold keys.
  - Amended 2026-11-05 at the user's request: without a hardware wallet, Saving can be a software wallet on this computer, keyed from the same recovery words (BIP84). Setup says a hardware wallet is safer.

### Flows

1. **Deposit**: coins arrive in Spending via an on-chain federation deposit address or a Lightning invoice.
2. **Target balance**: Spending keeps up to a target (default 0.05 BTC) for payments. New deposits and auto-change fill up to the target first. Anything above the target is split into denominations and queued for exit.
3. **Denominations**: 1-2-5 series from 0.001 to 1 BTC (0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1). Greedy split, largest first.
4. **Exit**: each queued denomination is withdrawn on its own to a fresh Saving address (see Exit timing).
5. **Saving spends**: the change output of every Saving send is a fresh federation deposit address, so change lands in Spending.
   - If the change is below the federation's minimum deposit, it stays in Saving, labelled "small change".
6. **Refill**: when Spending is below target, a single action sends the smallest Saving UTXOs that cover the gap into the federation.
   - Coin selection is smallest-first until the sum covers the gap.
   - No change output (it would link Saving to the refill). Any overshoot above target rejoins the exit queue.
   - Never automatic; always user-initiated and signed on the hardware wallet.
7. **Cover traffic**: the app periodically withdraws a denomination and re-deposits it to raise the anonymity set for all federation users. Paid from a monthly fee budget (default 20k sat).

### Exit timing

The exit rule must not depend on other users withdrawing the same amount first; otherwise nobody goes first.

- **Anonymity estimate** of a queued denomination: number of federation deposits (on-chain deposit inputs and Lightning receives) with amount ≥ that denomination, accepted since the user's deposit. Each could plausibly fund this withdrawal because ecash is fungible.
- **Eligible** once the minimum time since deposit has passed (default 24 h).
- **Online grace period**: no exit and no cover transaction within 10 minutes of the app starting or reconnecting to the federation.
- **Timing**: an eligible denomination exits after a random delay from a Poisson process (exponentially distributed waiting time). The rate increases with the anonymity estimate; the anonymity target (default 50) is the point where the expected wait becomes short. It is a soft target, not a gate.
- **Independence**: each denomination draws its own delay. Denominations from the same deposit never exit together.
- **Early exit**: manual per-denomination override ("Exit now (lower privacy)"). The resulting UTXO is flagged.
- **Fee guard**: automatic exits are deferred while the withdrawal fee exceeds 2% of the denomination. Manual early exits are not blocked; they show the fee.

### Cover traffic pacing

Cover transactions are spaced so the monthly budget does not run out under normal fee conditions:

- Before each cover transaction, compute `allowed_rate = remaining_budget / (estimated_fee_per_cover_tx × remaining_time_in_period)`.
- Draw the next cover time from a Poisson process at `allowed_rate`, with the fee estimate refreshed at each draw.
- Skip (and redraw) when the current fee estimate is above the fee guard. Fee spikes therefore delay cover traffic instead of draining the budget.
- Cover transactions obey the online grace period.

### Privacy records

- Each Saving UTXO stores: anonymity estimate at exit, source federation, and a flag for early exit / unmixed (received directly) / small change.
- Spending shows the current and attainable anonymity estimate for queued denominations.

### Defaults (editable in settings)

| Setting | Default |
|---|---|
| Spending target | 0.05 BTC |
| Anonymity target | 50 |
| Minimum time in pool | 24 h |
| Online grace period | 10 min |
| Fee guard | 2% of denomination |
| Cover budget | 20,000 sat / month |

## 2. UI

### Principles

- Show what most users need; nothing else. Detail is behind an amount with a `›` next to it: clicking the amount discloses its composition (UTXOs, notes, queue).
- Same layout pattern on both tabs: centered balance, centered Send / Receive, history below.

### Structure

- `AdwApplicationWindow` with an `AdwViewSwitcher` in the header (bottom switcher bar on narrow widths): **Saving** and **Spending**.
- Primary menu (⋮): Settings (federations, target, exit parameters, cover budget, hardware wallet, chain backend, proxy), About.

### Saving tab

- Device status line (e.g. "Ledger Nano S+").
- Balance with `›` → **Coins** page.
- "+X BTC depositing ›" in smaller text below the balance, only while funds are in flight to Saving; clicking switches to Spending.
- Send / Receive. Receive gives a direct Saving address; coins received this way are labelled "unmixed".
- History: incoming exits ("From Spending" + denomination tag), sends with the auto-change row nested beneath ("change → Spending"), direct receives.

### Spending tab

- Federation name.
- Balance with `›` → note/balance breakdown.
- Bar and caption "0.05 for spending · 0.42 moving to Saving ›"; the `›` opens the **Moving to Saving** page.
- Below target: caption becomes "0.0112 of 0.05 target · Refill from Saving".
- Send (Lightning invoice or on-chain address) / Receive (Lightning invoice or on-chain deposit address + QR).
- History heading row has a grey, right-aligned checkbox "Show cover traffic" (unchecked by default). Cover transactions are hidden unless checked.

### Pushed pages and dialogs

- **Coins** (from Saving balance): UTXO list with amount, privacy tag (anonymity estimate, early exit, unmixed, small change), date. Tap → detail (txid, address, source federation, anonymity at exit, freeze).
- **Moving to Saving** (from Spending): total, estimated attainable anonymity and typical exit time; one row per queued denomination with meter (current → attainable estimate); row menu → "Exit now (lower privacy)".
- **Refill dialog** (`AdwAlertDialog`): title, amount with `›`, "from Saving · fee ~N sat", Cancel / Confirm on <device>. Clicking the amount expands the selected UTXOs with privacy tags and the overshoot note.
- **Send review** (both tabs): amount, recipient, fee; `›` on the amount discloses inputs and where change goes.

### First run

1. Join a federation: suggested default or paste an invite code.
2. Spending is usable immediately. Saving tab shows an empty state: "Connect a hardware wallet".
3. When the Spending balance first exceeds the target, show a banner on Spending: "Set up Saving to move X BTC to cold storage · Set up ›". Until Saving exists, outbound denominations wait in Spending ("waiting for Saving").

### Desktop integration

- Closing the window keeps the app running in the background (XDG background portal). Quit via tray or menu.
- Tray icon via StatusNotifierItem (`ksni`): balances, Open, Quit. On stock GNOME this requires the AppIndicator extension; the app works without a tray.
- Desktop notifications: deposit confirmed, exit landed in Saving, send confirmed, Saving setup nudge.

## 3. Architecture

### Crates

Cargo workspace:

- `fedimix-core`: no GTK dependency. All wallet, federation, scheduling and persistence logic.
- `fedimix-gtk`: libadwaita UI, tray, notifications, background portal. Binary `fedimix`.

A future `fedimix-ffi` crate wraps the core with UniFFI for Android/iOS. The core's public API therefore uses owned, FFI-friendly types: no generics or borrowed lifetimes in public signatures.

### Core API shape

- `Fedimix` handle: async methods (`join_federation`, `receive`, `send`, `refill`, `exit_now`, `set_settings`, …).
- `subscribe()` → stream of `Event`s (balance changed, history item added/updated, queue changed, notification-worthy events).
- Snapshot types for initial render (`SavingState`, `SpendingState`, `QueueState`).

### Threading

The core runs on a tokio multi-thread runtime in its own thread. The GTK side calls core methods via a handle that spawns onto that runtime, and consumes `Event`s through an `async-channel` read with `glib::spawn_future_local`.

### Core modules

- `federation`: multi-federation wrapper around `fedimint-client` with mint, wallet (on-chain deposit/withdraw) and LNv2 modules. One client DB per federation.
- `saving`: BDK watch-only wallet from the hardware wallet xpub. Signing through `async-hwi` (Ledger, Trezor, Coldcard, Jade, BitBox). Builds PSBTs whose change output is a fresh federation deposit address.
- `anonymity`: incrementally syncs the federation's public signed session history (as fedimint-observer does), starting at join time. Counts wallet-module deposit inputs and Lightning receives by amount and session, and answers "deposits ≥ D since session S".
- `scheduler`: persisted state machine per queued denomination: `Queued → Eligible → Scheduled(at) → Withdrawing(txid) → Landed`. Delays are redrawn on every start (Poisson is memoryless). Enforces the online grace period, fee guard, target balance, refill selection and cover-traffic pacing. Clock and RNG are injected.
- `store`: SQLite via `rusqlite`: BDK wallet persistence, UTXO privacy labels, scheduler queue, settings, history metadata.

### Chain access

Esplora or Electrum, configurable. Default: a public Esplora server. Optional SOCKS5 proxy (e.g. Tor) for chain and federation connections.

## 4. Error handling

- **Federation unreachable**: "Offline" banner on Spending, scheduler paused; grace period restarts on reconnect.
- **Hardware wallet missing**: exits don't need it (addresses derive from the xpub). Send and Refill dialogs include a "Connect your <device>" step.
- **Change below minimum deposit**: stays in Saving as "small change"; mentioned only in the send review details.
- **Fee spike**: automatic exits and cover traffic defer (fee guard).
- **Withdrawal fees**: paid from the Spending balance. If Spending cannot cover the fee for an exit, the exit waits and Spending shows a hint.
- **Crash/restart**: scheduler state in SQLite; Fedimint state machines resume. Exits always use a freshly derived Saving address and are never retried to a previously used one.

## 5. Testing

- **Core unit tests** with injected clock and RNG: denomination split, target/queue logic, refill selection, Poisson scheduling and grace period, cover pacing (budget not exhausted across a simulated month at constant fees), anonymity counting over fixture session histories.
- **Integration tests** with devimint (local federation, regtest bitcoind, Lightning gateway) from the Nix flake: deposit → exit → Saving; Saving send → change in Spending; refill. A BDK software signer stands in for the hardware wallet in tests only.
- **Demo mode**: `fedimix --demo` runs the GTK UI against a fake core backend with scripted data, for UI iteration before the Fedimint integration exists.

## 6. Build phases

1. Workspace split, core API types and events, GTK shell (both tabs, pushed pages, dialogs) against the demo backend.
2. Spending: federation join, receive/send via Lightning and on-chain, history.
3. Saving: xpub import via hardware wallet, BDK sync, send with change-to-federation, refill.
4. Scheduler and anonymity estimator: denomination queue, Poisson exits, early exit, privacy labels.
5. Cover traffic, tray icon, notifications, background running.

## Out of scope for v1

- Android/iOS apps (core API is designed for them).
- Automatic spreading of one deposit across multiple federations.
- Built-in Tor (use a SOCKS5 proxy).
- Software signer for Saving outside tests.
