//! In-memory demo backend with mockup scenarios.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::api::*;
use crate::backend::Backend;
use crate::destination::{DestinationKind, parse_destination};
use crate::error::{FedimixError, Result};
use crate::event::{Event, EventStream, Notice};

mod fixtures;
use fixtures::BASE_TS;

const ONCHAIN_FEE_SAT: u64 = 420;
const LIGHTNING_FEE_SAT: u64 = 12;
const SMALL_CHANGE_SAT: u64 = 10_000;
/// Virtual size of a demo Saving transaction; 420 sat at 3 sat/vB.
const TX_VBYTES: u64 = 140;
const NOTIFY_DELAY: Duration = Duration::from_secs(2);
const OPERATION_STEP: Duration = Duration::from_millis(300);
const PREVIEW_TTL: Duration = Duration::from_secs(600);
/// Spec section 1: the 1-2-5 series from 1 BTC down to 0.001.
const EXIT_DENOMINATIONS_SAT: [u64; 10] = [
    100_000_000,
    50_000_000,
    20_000_000,
    10_000_000,
    5_000_000,
    2_000_000,
    1_000_000,
    500_000,
    200_000,
    100_000,
];
/// Operation states before the job takes effect.
const SIGNED_PLAN: &[OperationState] = &[
    OperationState::AwaitingDevice,
    OperationState::Signing,
    OperationState::Broadcasting,
];
const UNSIGNED_PLAN: &[OperationState] = &[OperationState::Broadcasting];
/// Largest first; used to split a balance into notes.
const DENOMINATIONS_SAT: [u64; 13] = [
    10_000_000, 5_000_000, 2_000_000, 1_000_000, 500_000, 200_000, 100_000, 50_000, 20_000, 10_000,
    2_000, 1_000, 1,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoScenario {
    Funded,
    BelowTarget,
    NoSaving,
    FirstRun,
    Offline,
    /// Spending too low for exit fees, every queue deferral and history status.
    ExitDeferred,
    /// Below target with a queue, and names far longer than any screen.
    LongNames,
    /// Saving set up, its device unplugged.
    DeviceDisconnected,
    /// Funded, with previews that expire within a second.
    PreviewExpiry,
    /// Funded, with a device that refuses to sign and to set up.
    DeviceRejects,
}

/// How fast the demo moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemoTiming {
    /// Time spent in each operation state.
    pub step: Duration,
    pub preview_ttl: Duration,
}

impl DemoTiming {
    fn for_scenario(scenario: DemoScenario) -> Self {
        DemoTiming {
            step: OPERATION_STEP,
            preview_ttl: match scenario {
                DemoScenario::PreviewExpiry => Duration::from_secs(1),
                _ => PREVIEW_TTL,
            },
        }
    }
}

impl FromStr for DemoScenario {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "funded" => Ok(Self::Funded),
            "below-target" => Ok(Self::BelowTarget),
            "no-saving" => Ok(Self::NoSaving),
            "first-run" => Ok(Self::FirstRun),
            "offline" => Ok(Self::Offline),
            "exit-deferred" => Ok(Self::ExitDeferred),
            "long-names" => Ok(Self::LongNames),
            "device-disconnected" => Ok(Self::DeviceDisconnected),
            "preview-expiry" => Ok(Self::PreviewExpiry),
            "device-rejects" => Ok(Self::DeviceRejects),
            other => Err(format!(
                "unknown scenario '{other}' (expected funded, below-target, no-saving, first-run, offline, exit-deferred, long-names, device-disconnected, preview-expiry, device-rejects)"
            )),
        }
    }
}

type Senders = Arc<StdMutex<Vec<async_channel::Sender<Event>>>>;

/// Send `event` to every live subscriber; closed ones are dropped.
fn broadcast(senders: &Senders, event: &Event) {
    let mut list = senders.lock().unwrap_or_else(PoisonError::into_inner);
    list.retain(|tx| tx.try_send(event.clone()).is_ok());
}

struct State {
    device: Option<DeviceInfo>,
    detected: Vec<DeviceInfo>,
    suggested: Vec<SuggestedFederation>,
    in_flight: Amount,
    coins: Vec<Coin>,
    saving_history: Vec<HistoryItem>,
    federation: Option<FederationInfo>,
    spending_balance: Amount,
    for_spending: Amount,
    moving: Amount,
    saving_ready: bool,
    spending_history: Vec<HistoryItem>,
    queue: Vec<QueueItem>,
    waiting_for_saving: bool,
    settings: Settings,
    sends: Previews<SendPreview>,
    refills: Previews<RefillPreview>,
    exits: Previews<ExitPreview>,
    operations: HashMap<OperationId, RunningOperation>,
    /// Previews whose operation was cancelled.
    cancelled: HashSet<String>,
    next_id: u64,
}

/// Unconfirmed previews with their expiry.
type Previews<T> = HashMap<String, (Timestamp, T)>;

#[derive(Default)]
struct RunningOperation {
    cancel_requested: bool,
}

/// Remove a preview for confirmation.
fn take_preview<T>(
    previews: &mut Previews<T>,
    cancelled: &HashSet<String>,
    id: &str,
) -> Result<(Timestamp, T)> {
    if cancelled.contains(id) {
        return Err(FedimixError::Cancelled);
    }
    let held = previews
        .remove(id)
        .ok_or_else(|| FedimixError::UnknownItem(id.to_string()))?;
    if held.0 <= now_secs() {
        return Err(FedimixError::PreviewExpired);
    }
    Ok(held)
}

/// What an operation does once it gets through its states.
enum Job {
    Send(SendPreview),
    Refill(RefillPreview),
    Exit {
        preview: ExitPreview,
        /// Queue status to go back to if the exit fails.
        restore: QueueItemStatus,
    },
}

impl Job {
    fn kind(&self) -> OperationKind {
        match self {
            Job::Send(_) => OperationKind::Send,
            Job::Refill(_) => OperationKind::Refill,
            Job::Exit { .. } => OperationKind::Exit,
        }
    }

    /// The state change, which happens at the end of the operation.
    fn apply(&self, st: &mut State) -> Result<Vec<Event>> {
        match self {
            Job::Send(p) => st.apply_send(p),
            Job::Refill(p) => st.apply_refill(p),
            Job::Exit { preview, .. } => st.apply_exit(preview),
        }
    }

    /// Undo what starting the job did; nothing else has happened yet.
    fn abandon(self, st: &mut State) -> Vec<Event> {
        let Job::Exit { preview, restore } = self else {
            return Vec::new();
        };
        if let Some(item) = st.queue.iter_mut().find(|i| i.id == preview.item_id) {
            item.status = restore;
        }
        vec![Event::QueueChanged]
    }
}

/// One operation's trip through its states, on its own task.
struct Run {
    state: Arc<Mutex<State>>,
    senders: Senders,
    step: Duration,
    rejects: bool,
    id: OperationId,
    preview_id: String,
    job: Job,
    plan: &'static [OperationState],
}

impl Run {
    fn emit(&self, state: OperationState) {
        let event = Event::Operation {
            id: self.id.clone(),
            kind: self.job.kind(),
            state,
        };
        broadcast(&self.senders, &event);
    }

    async fn cancel_requested(&self) -> bool {
        let st = self.state.lock().await;
        st.operations
            .get(&self.id)
            .is_some_and(|op| op.cancel_requested)
    }

    /// Walk the plan; `Some` is an early final state.
    async fn walk(&self) -> Option<OperationState> {
        for state in self.plan {
            self.emit(state.clone());
            tokio::time::sleep(self.step).await;
            if *state == OperationState::Broadcasting {
                break;
            }
            if self.cancel_requested().await {
                return Some(OperationState::Cancelled);
            }
            if self.rejects && *state == OperationState::Signing {
                return Some(OperationState::Failed {
                    error: FedimixError::DeviceRejected,
                });
            }
        }
        None
    }

    async fn run(self) {
        let early = self.walk().await;
        let mut st = self.state.lock().await;
        st.operations.remove(&self.id);
        let (events, last) = match early {
            None => match self.job.apply(&mut st) {
                Ok(events) => (events, OperationState::Done),
                Err(error) => (Vec::new(), OperationState::Failed { error }),
            },
            Some(early) => (Vec::new(), early),
        };
        if last == OperationState::Cancelled {
            st.cancelled.insert(self.preview_id.clone());
        }
        drop(st);
        for event in &events {
            broadcast(&self.senders, event);
        }
        if let Job::Send(p) = &self.job
            && last == OperationState::Done
        {
            notify_later(&self.senders, Notice::SendConfirmed { amount: p.amount });
        }
        self.finish(last).await;
    }

    async fn finish(self, last: OperationState) {
        self.emit(last.clone());
        if last != OperationState::Done {
            let mut st = self.state.lock().await;
            let events = self.job.abandon(&mut st);
            drop(st);
            for event in &events {
                broadcast(&self.senders, event);
            }
        }
    }
}

impl State {
    fn new(scenario: DemoScenario) -> Self {
        let f = fixtures::fixture(scenario);
        State {
            device: f.device,
            detected: f.detected,
            suggested: f.suggested,
            in_flight: f.in_flight,
            coins: f.coins,
            saving_history: f.saving_history,
            federation: f.federation,
            spending_balance: f.spending_balance,
            for_spending: f.for_spending,
            moving: f.moving,
            saving_ready: f.saving_ready,
            spending_history: f.spending_history,
            queue: f.queue,
            waiting_for_saving: f.waiting_for_saving,
            settings: Settings::default(),
            sends: HashMap::new(),
            refills: HashMap::new(),
            exits: HashMap::new(),
            operations: HashMap::new(),
            cancelled: HashSet::new(),
            next_id: 1,
        }
    }

    fn saving_balance(&self) -> Amount {
        sum(self.coins.iter().map(|c| c.amount))
    }

    fn fresh_id(&mut self, prefix: &str) -> String {
        let id = format!("{prefix}-{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn history_item(&mut self, kind: HistoryKind, amount: Amount, incoming: bool) -> HistoryItem {
        HistoryItem {
            id: self.fresh_id("h"),
            kind,
            amount,
            incoming,
            status: HistoryStatus::Complete,
            timestamp: BASE_TS,
        }
    }

    fn notes(&self) -> Vec<NoteBucket> {
        let mut rest = self.spending_balance.as_sat();
        let mut notes = Vec::new();
        for denom in DENOMINATIONS_SAT {
            let count = rest / denom;
            if count > 0 {
                notes.push(NoteBucket {
                    denomination: Amount::from_sat(denom),
                    count: count as u32,
                });
                rest %= denom;
            }
        }
        debug_assert_eq!(rest, 0, "balance not representable in demo denominations");
        notes
    }

    fn next_coin_index(&mut self) -> usize {
        let n = self.next_id;
        self.next_id += 1;
        100 + n as usize
    }

    fn check_online(&self) -> Result<()> {
        match &self.federation {
            Some(f) if !f.online => Err(FedimixError::Offline),
            _ => Ok(()),
        }
    }

    fn check_device(&self) -> Result<()> {
        match &self.device {
            None => Err(FedimixError::NotSetUp("Set up Saving first".to_string())),
            Some(d) if !d.connected => Err(FedimixError::DeviceNotConnected(d.name.clone())),
            Some(_) => Ok(()),
        }
    }

    /// Whether the preview can still be carried out.
    fn check_send(&self, p: &SendPreview) -> Result<()> {
        match p.wallet {
            WalletKind::Spending => {
                self.check_online()?;
                let needed = with_fee(p.amount, p.fee);
                if needed > self.spending_balance {
                    return Err(FedimixError::InsufficientFunds {
                        available: self.spending_balance,
                        needed,
                    });
                }
                Ok(())
            }
            WalletKind::Saving => {
                self.check_device()?;
                let inputs = p.inputs.iter().map(|i| i.outpoint_or_note.as_str());
                self.check_coins(inputs)
            }
        }
    }

    fn check_refill(&self, p: &RefillPreview) -> Result<()> {
        self.check_device()?;
        self.check_coins(p.coins.iter().map(|c| c.outpoint.as_str()))
    }

    fn check_coins<'a>(&self, mut outpoints: impl Iterator<Item = &'a str>) -> Result<()> {
        match outpoints.find(|o| !self.coins.iter().any(|c| c.outpoint == *o)) {
            Some(gone) => Err(FedimixError::UnknownItem(gone.to_string())),
            None => Ok(()),
        }
    }

    fn apply_send(&mut self, p: &SendPreview) -> Result<Vec<Event>> {
        self.check_send(p)?;
        let mut events = Vec::new();
        match p.wallet {
            WalletKind::Spending => {
                let total = with_fee(p.amount, p.fee);
                self.spending_balance = self.spending_balance.saturating_sub(total);
                self.for_spending = self.for_spending.saturating_sub(total);
                let lightning = parse_destination(&p.destination)
                    .is_ok_and(|d| d.kind == DestinationKind::Lightning);
                let kind = if lightning {
                    HistoryKind::Paid
                } else {
                    HistoryKind::Sent { change: None }
                };
                let item = self.history_item(kind, p.amount, false);
                self.spending_history.insert(0, item.clone());
                events.push(Event::BalanceChanged {
                    wallet: WalletKind::Spending,
                    balance: self.spending_balance,
                });
                events.push(Event::HistoryItemAdded {
                    wallet: WalletKind::Spending,
                    item,
                });
            }
            WalletKind::Saving => {
                self.coins
                    .retain(|c| !p.inputs.iter().any(|i| i.outpoint_or_note == c.outpoint));
                let sent_change = match &p.change {
                    ChangeDestination::None => None,
                    ChangeDestination::Spending { amount } => {
                        self.spending_balance = sum([self.spending_balance, *amount].into_iter());
                        self.for_spending = sum([self.for_spending, *amount].into_iter());
                        events.push(Event::BalanceChanged {
                            wallet: WalletKind::Spending,
                            balance: self.spending_balance,
                        });
                        Some(SentChange {
                            amount: *amount,
                            to_spending: true,
                        })
                    }
                    ChangeDestination::SmallChange { amount } => {
                        let n = self.next_coin_index();
                        self.coins.push(Coin {
                            outpoint: fixtures::outpoint_for(n),
                            amount: *amount,
                            tag: PrivacyTag::SmallChange,
                            received_at: BASE_TS,
                            address: fixtures::address_for(n),
                            source_federation: None,
                            frozen: false,
                        });
                        Some(SentChange {
                            amount: *amount,
                            to_spending: false,
                        })
                    }
                };
                let item = self.history_item(
                    HistoryKind::Sent {
                        change: sent_change,
                    },
                    p.amount,
                    false,
                );
                self.saving_history.insert(0, item.clone());
                events.push(Event::BalanceChanged {
                    wallet: WalletKind::Saving,
                    balance: self.saving_balance(),
                });
                events.push(Event::HistoryItemAdded {
                    wallet: WalletKind::Saving,
                    item,
                });
            }
        }
        Ok(events)
    }

    fn apply_refill(&mut self, p: &RefillPreview) -> Result<Vec<Event>> {
        self.check_refill(p)?;
        self.coins
            .retain(|c| !p.coins.iter().any(|r| r.outpoint == c.outpoint));
        let arrived = p.amount.saturating_sub(p.fee);
        self.spending_balance = sum([self.spending_balance, arrived].into_iter());
        // The overshoot leaves again in denominations; what is too small for
        // one stays in Spending.
        let denominations = split_denominations(p.overshoot);
        let queued = sum(denominations.iter().copied());
        self.for_spending = sum([self.for_spending, arrived.saturating_sub(queued)].into_iter());
        self.moving = sum([self.moving, queued].into_iter());
        for denomination in denominations {
            let item = QueueItem {
                id: self.fresh_id("queue"),
                denomination,
                status: QueueItemStatus::Queued,
                anonymity_current: 0,
                anonymity_attainable: self.settings.anonymity_target,
            };
            self.queue.push(item);
        }
        let out = self.history_item(HistoryKind::Sent { change: None }, p.amount, false);
        let inc = self.history_item(HistoryKind::Deposit, arrived, true);
        self.saving_history.insert(0, out.clone());
        self.spending_history.insert(0, inc.clone());
        Ok(vec![
            Event::BalanceChanged {
                wallet: WalletKind::Saving,
                balance: self.saving_balance(),
            },
            Event::HistoryItemAdded {
                wallet: WalletKind::Saving,
                item: out,
            },
            Event::BalanceChanged {
                wallet: WalletKind::Spending,
                balance: self.spending_balance,
            },
            Event::HistoryItemAdded {
                wallet: WalletKind::Spending,
                item: inc,
            },
            Event::QueueChanged,
        ])
    }

    fn apply_exit(&mut self, p: &ExitPreview) -> Result<Vec<Event>> {
        let pos = self
            .queue
            .iter()
            .position(|i| i.id == p.item_id)
            .ok_or_else(|| FedimixError::UnknownItem(p.item_id.clone()))?;
        let item = self.queue.remove(pos);
        let denom = item.denomination;
        self.moving = self.moving.saturating_sub(denom);
        self.spending_balance = self.spending_balance.saturating_sub(with_fee(denom, p.fee));
        self.for_spending = self.for_spending.saturating_sub(p.fee);
        self.in_flight = self.in_flight.saturating_sub(denom);
        let n = self.next_coin_index();
        let source_federation = self.federation.as_ref().map(|f| f.name.clone());
        self.coins.push(Coin {
            outpoint: fixtures::outpoint_for(n),
            amount: denom,
            tag: PrivacyTag::EarlyExit {
                estimate: item.anonymity_current,
            },
            received_at: BASE_TS,
            address: fixtures::address_for(n),
            source_federation,
            frozen: false,
        });
        let spending_item = self.history_item(
            HistoryKind::ToSaving {
                denomination: denom,
            },
            denom,
            false,
        );
        let saving_item = self.history_item(
            HistoryKind::FromSpending {
                denomination: denom,
            },
            denom,
            true,
        );
        self.spending_history.insert(0, spending_item.clone());
        self.saving_history.insert(0, saving_item.clone());
        Ok(vec![
            Event::QueueChanged,
            Event::BalanceChanged {
                wallet: WalletKind::Spending,
                balance: self.spending_balance,
            },
            Event::HistoryItemAdded {
                wallet: WalletKind::Spending,
                item: spending_item,
            },
            Event::BalanceChanged {
                wallet: WalletKind::Saving,
                balance: self.saving_balance(),
            },
            Event::HistoryItemAdded {
                wallet: WalletKind::Saving,
                item: saving_item,
            },
            Event::Notification(Notice::ExitLanded { amount: denom }),
        ])
    }
}

/// Greedy split into exit denominations, largest first; the rest is too
/// small for any of them.
fn split_denominations(amount: Amount) -> Vec<Amount> {
    let mut rest = amount.as_sat();
    let mut out = Vec::new();
    for denom in EXIT_DENOMINATIONS_SAT {
        while rest >= denom {
            rest -= denom;
            out.push(Amount::from_sat(denom));
        }
    }
    out
}

fn now_secs() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn sum(amounts: impl Iterator<Item = Amount>) -> Amount {
    amounts.fold(Amount::ZERO, |a, b| Amount {
        msat: a.msat.saturating_add(b.msat),
    })
}

fn with_fee(amount: Amount, fee: Amount) -> Amount {
    sum([amount, fee].into_iter())
}

pub struct DemoBackend {
    state: Arc<Mutex<State>>,
    senders: Senders,
    timing: DemoTiming,
    rejects: bool,
}

impl DemoBackend {
    pub fn new(scenario: DemoScenario) -> Arc<Self> {
        Self::with_timing(scenario, DemoTiming::for_scenario(scenario))
    }

    pub fn with_timing(scenario: DemoScenario, timing: DemoTiming) -> Arc<Self> {
        Arc::new(DemoBackend {
            state: Arc::new(Mutex::new(State::new(scenario))),
            senders: Arc::new(StdMutex::new(Vec::new())),
            timing,
            rejects: scenario == DemoScenario::DeviceRejects,
        })
    }

    fn emit(&self, event: Event) {
        broadcast(&self.senders, &event);
    }

    fn preview_expiry(&self) -> Timestamp {
        now_secs() + self.timing.preview_ttl.as_secs()
    }

    /// Start `job` on its own task. Needs a tokio runtime.
    fn start(
        &self,
        st: &mut State,
        preview_id: String,
        job: Job,
        plan: &'static [OperationState],
    ) -> Result<OperationId> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| FedimixError::Internal(e.to_string()))?;
        let id = st.fresh_id("op");
        st.operations
            .insert(id.clone(), RunningOperation::default());
        let run = Run {
            state: Arc::clone(&self.state),
            senders: Arc::clone(&self.senders),
            step: self.timing.step,
            rejects: self.rejects,
            id: id.clone(),
            preview_id,
            job,
            plan,
        };
        handle.spawn(run.run());
        Ok(id)
    }
}

/// Emit `notice` after a short delay (no-op without a tokio runtime).
fn notify_later(senders: &Senders, notice: Notice) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let senders = Arc::clone(senders);
    handle.spawn(async move {
        tokio::time::sleep(NOTIFY_DELAY).await;
        broadcast(&senders, &Event::Notification(notice));
    });
}

/// The amount to send, given what the destination carries. `Max` stays
/// unresolved here: it depends on the wallet's balance and fee.
fn requested_amount(requested: SendAmount, carried: Option<Amount>) -> Result<SendAmount> {
    let amount = match (requested, carried) {
        (SendAmount::FromInvoice, Some(a)) => SendAmount::Exact(a),
        (SendAmount::FromInvoice, None) => {
            return Err(FedimixError::InvalidAmount(
                "This destination has no amount: enter one".to_string(),
            ));
        }
        (SendAmount::Exact(a), Some(b)) if a != b => {
            return Err(FedimixError::InvalidAmount(
                "This destination asks for a different amount".to_string(),
            ));
        }
        (SendAmount::Max, Some(_)) => {
            return Err(FedimixError::InvalidAmount(
                "This destination has a fixed amount".to_string(),
            ));
        }
        (other, _) => other,
    };
    if amount == SendAmount::Exact(Amount::ZERO) {
        return Err(FedimixError::InvalidAmount(
            "Amount must be above zero".to_string(),
        ));
    }
    Ok(amount)
}

/// Resolve `Max` against `available`; `fee` comes off the top.
fn resolve_amount(amount: SendAmount, available: Amount, fee: Amount) -> Result<Amount> {
    match amount {
        SendAmount::Exact(a) => Ok(a),
        _ => available
            .checked_sub(fee)
            .filter(|a| *a != Amount::ZERO)
            .ok_or(FedimixError::InsufficientFunds {
                available,
                needed: fee,
            }),
    }
}

#[async_trait]
impl Backend for DemoBackend {
    fn subscribe(&self) -> EventStream {
        let (tx, rx) = async_channel::unbounded();
        self.senders
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tx);
        EventStream::new(rx)
    }

    async fn saving_state(&self) -> Result<SavingState> {
        let st = self.state.lock().await;
        Ok(SavingState {
            device: st.device.clone(),
            balance: st.saving_balance(),
            in_flight: st.in_flight,
            coins: st.coins.clone(),
            history: st.saving_history.clone(),
        })
    }

    async fn spending_state(&self) -> Result<SpendingState> {
        let st = self.state.lock().await;
        Ok(SpendingState {
            federation: st.federation.clone(),
            balance: st.spending_balance,
            target: st.settings.spending_target,
            for_spending: st.for_spending,
            moving: st.moving,
            saving_ready: st.saving_ready,
            notes: st.notes(),
            history: st.spending_history.clone(),
        })
    }

    async fn queue_state(&self) -> Result<QueueState> {
        let st = self.state.lock().await;
        Ok(QueueState {
            total: sum(st.queue.iter().map(|i| i.denomination)),
            anonymity_estimate: st.federation.as_ref().map_or(0, |f| f.anonymity_estimate),
            typical_exit_secs: 86_400,
            waiting_for_saving: st.waiting_for_saving,
            items: st.queue.clone(),
        })
    }

    async fn settings(&self) -> Result<Settings> {
        Ok(self.state.lock().await.settings.clone())
    }

    async fn set_settings(&self, settings: Settings) -> Result<()> {
        self.state.lock().await.settings = settings;
        self.emit(Event::SettingsChanged);
        Ok(())
    }

    async fn suggested_federations(&self) -> Result<Vec<SuggestedFederation>> {
        Ok(self.state.lock().await.suggested.clone())
    }

    async fn join_federation(&self, invite: String) -> Result<FederationInfo> {
        if !invite.starts_with("fed1") {
            return Err(FedimixError::InvalidInvite(
                "invite must start with fed1".to_string(),
            ));
        }
        let info = fixtures::federation(true);
        self.state.lock().await.federation = Some(info.clone());
        self.emit(Event::FederationChanged);
        Ok(info)
    }

    async fn federations(&self) -> Result<Vec<FederationInfo>> {
        Ok(self.state.lock().await.federation.iter().cloned().collect())
    }

    async fn set_active_federation(&self, id: String) -> Result<()> {
        let known = self
            .state
            .lock()
            .await
            .federation
            .as_ref()
            .is_some_and(|f| f.id == id);
        if !known {
            return Err(FedimixError::UnknownItem(id));
        }
        self.emit(Event::FederationChanged);
        Ok(())
    }

    async fn detect_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self.state.lock().await.detected.clone())
    }

    async fn setup_saving(&self, device_id: String) -> Result<()> {
        let Some(device) = self
            .state
            .lock()
            .await
            .detected
            .iter()
            .find(|d| d.id == device_id)
            .cloned()
        else {
            return Err(FedimixError::UnknownItem(device_id));
        };
        if self.rejects {
            return Err(FedimixError::DeviceRejected);
        }
        if !device.connected {
            return Err(FedimixError::DeviceNotConnected(device.name));
        }
        {
            let mut st = self.state.lock().await;
            st.device = Some(device);
            st.saving_ready = true;
            st.waiting_for_saving = false;
        }
        self.emit(Event::SavingChanged);
        self.emit(Event::QueueChanged);
        Ok(())
    }

    async fn receive(&self, request: ReceiveRequest) -> Result<ReceiveInfo> {
        if request.wallet == WalletKind::Spending {
            let st = self.state.lock().await;
            if st.federation.as_ref().is_some_and(|f| !f.online) {
                return Err(FedimixError::Offline);
            }
        }
        Ok(match request.method {
            ReceiveMethod::Lightning {
                amount,
                expiry_secs,
                ..
            } => {
                let payload = format!("lnbc{}n1pdemo0fedimix0invoice", amount.as_sat() * 10);
                ReceiveInfo {
                    uri: format!("lightning:{payload}"),
                    payload,
                    expires_at: Some(now_secs() + u64::from(expiry_secs)),
                }
            }
            ReceiveMethod::OnChain => {
                let payload = fixtures::address_for(self.state.lock().await.next_coin_index());
                ReceiveInfo {
                    uri: format!("bitcoin:{payload}"),
                    payload,
                    expires_at: None,
                }
            }
        })
    }

    async fn prepare_send(&self, request: SendRequest) -> Result<SendPreview> {
        let SendRequest {
            wallet,
            destination,
            amount,
            fee_rate_sat_vb,
        } = request;
        let destination = parse_destination(&destination)?;
        let lightning = destination.kind == DestinationKind::Lightning;
        if wallet == WalletKind::Saving && lightning {
            return Err(FedimixError::InvalidDestination(
                "Saving can only send to a bitcoin address".to_string(),
            ));
        }
        let amount = requested_amount(amount, destination.amount)?;
        let fee = Amount::from_sat(match (wallet, fee_rate_sat_vb) {
            _ if lightning => LIGHTNING_FEE_SAT,
            (WalletKind::Saving, Some(rate)) => rate.saturating_mul(TX_VBYTES),
            _ => ONCHAIN_FEE_SAT,
        });
        let destination = destination.normalized;
        let mut st = self.state.lock().await;
        let (amount, inputs, change) = match wallet {
            WalletKind::Spending => {
                match &st.federation {
                    None => return Err(FedimixError::NotSetUp("Join a federation first".into())),
                    Some(f) if !f.online => return Err(FedimixError::Offline),
                    Some(_) => {}
                }
                let amount = resolve_amount(amount, st.spending_balance, fee)?;
                let needed = with_fee(amount, fee);
                if needed > st.spending_balance {
                    return Err(FedimixError::InsufficientFunds {
                        available: st.spending_balance,
                        needed,
                    });
                }
                let input = SendInput {
                    outpoint_or_note: "notes".to_string(),
                    amount: needed,
                    tag: None,
                };
                (amount, vec![input], ChangeDestination::None)
            }
            WalletKind::Saving => {
                let mut candidates: Vec<&Coin> = st.coins.iter().filter(|c| !c.frozen).collect();
                let available = sum(candidates.iter().map(|c| c.amount));
                let amount = resolve_amount(amount, available, fee)?;
                let needed = with_fee(amount, fee);
                if needed > available {
                    return Err(FedimixError::InsufficientFunds { available, needed });
                }
                candidates.sort_by_key(|c| std::cmp::Reverse(c.amount));
                let mut inputs = Vec::new();
                let mut total = Amount::ZERO;
                for c in candidates {
                    if total >= needed {
                        break;
                    }
                    total = sum([total, c.amount].into_iter());
                    inputs.push(SendInput {
                        outpoint_or_note: c.outpoint.clone(),
                        amount: c.amount,
                        tag: Some(c.tag.clone()),
                    });
                }
                let change = total.saturating_sub(needed);
                let change = if change == Amount::ZERO {
                    ChangeDestination::None
                } else if change.as_sat() < SMALL_CHANGE_SAT {
                    ChangeDestination::SmallChange { amount: change }
                } else {
                    ChangeDestination::Spending { amount: change }
                };
                (amount, inputs, change)
            }
        };
        let preview = SendPreview {
            id: st.fresh_id("send"),
            wallet,
            destination,
            amount,
            fee,
            inputs,
            change,
            expires_at: self.preview_expiry(),
        };
        let held = (preview.expires_at, preview.clone());
        st.sends.insert(preview.id.clone(), held);
        Ok(preview)
    }

    async fn confirm_send(&self, preview_id: String) -> Result<OperationId> {
        let mut guard = self.state.lock().await;
        let st = &mut *guard;
        let held = take_preview(&mut st.sends, &st.cancelled, &preview_id)?;
        if let Err(e) = st.check_send(&held.1) {
            st.sends.insert(preview_id, held);
            return Err(e);
        }
        let (_, preview) = held;
        let plan = if preview.wallet == WalletKind::Saving {
            SIGNED_PLAN
        } else {
            UNSIGNED_PLAN
        };
        self.start(st, preview_id, Job::Send(preview), plan)
    }

    async fn prepare_exit(&self, item_id: String) -> Result<ExitPreview> {
        let mut st = self.state.lock().await;
        let item = st
            .queue
            .iter()
            .find(|i| i.id == item_id)
            .ok_or_else(|| FedimixError::UnknownItem(item_id.clone()))?;
        if matches!(
            item.status,
            QueueItemStatus::Withdrawing { .. } | QueueItemStatus::Landed
        ) {
            return Err(FedimixError::UnknownItem(item_id));
        }
        if st.device.is_none() {
            return Err(FedimixError::NotSetUp("Set up Saving first".to_string()));
        }
        st.check_online()?;
        let fee = Amount::from_sat(ONCHAIN_FEE_SAT);
        if st.for_spending < fee {
            return Err(FedimixError::InsufficientFunds {
                available: st.for_spending,
                needed: fee,
            });
        }
        let preview = ExitPreview {
            id: st.fresh_id("exit"),
            item_id,
            fee,
            expires_at: self.preview_expiry(),
        };
        let held = (preview.expires_at, preview.clone());
        st.exits.insert(preview.id.clone(), held);
        Ok(preview)
    }

    async fn exit_now(&self, preview_id: String) -> Result<OperationId> {
        let mut guard = self.state.lock().await;
        let st = &mut *guard;
        let (_, preview) = take_preview(&mut st.exits, &st.cancelled, &preview_id)?;
        st.check_online()?;
        let txid = fixtures::outpoint_for(st.next_coin_index());
        let item = st
            .queue
            .iter_mut()
            .find(|i| i.id == preview.item_id)
            .filter(|i| !matches!(i.status, QueueItemStatus::Withdrawing { .. }))
            .ok_or_else(|| FedimixError::UnknownItem(preview.item_id.clone()))?;
        let restore = std::mem::replace(&mut item.status, QueueItemStatus::Withdrawing { txid });
        let id = self.start(
            st,
            preview_id,
            Job::Exit { preview, restore },
            UNSIGNED_PLAN,
        )?;
        self.emit(Event::QueueChanged);
        Ok(id)
    }

    async fn discard_preview(&self, preview_id: String) -> Result<()> {
        let mut st = self.state.lock().await;
        st.sends.remove(&preview_id);
        st.refills.remove(&preview_id);
        st.exits.remove(&preview_id);
        Ok(())
    }

    async fn cancel_operation(&self, id: OperationId) -> Result<()> {
        let mut st = self.state.lock().await;
        st.operations
            .get_mut(&id)
            .ok_or(FedimixError::UnknownItem(id))?
            .cancel_requested = true;
        Ok(())
    }

    async fn confirm_refill(&self, preview_id: String) -> Result<OperationId> {
        let mut guard = self.state.lock().await;
        let st = &mut *guard;
        let held = take_preview(&mut st.refills, &st.cancelled, &preview_id)?;
        if let Err(e) = st.check_refill(&held.1) {
            st.refills.insert(preview_id, held);
            return Err(e);
        }
        self.start(st, preview_id, Job::Refill(held.1), SIGNED_PLAN)
    }

    async fn prepare_refill(&self) -> Result<RefillPreview> {
        let mut st = self.state.lock().await;
        let device = st
            .device
            .clone()
            .ok_or_else(|| FedimixError::NotSetUp("Set up Saving first".to_string()))?;
        let target = st.settings.spending_target;
        if st.for_spending >= target {
            return Err(FedimixError::InvalidAmount(
                "Spending is at target".to_string(),
            ));
        }
        let gap = target.saturating_sub(st.for_spending);
        let fee = Amount::from_sat(ONCHAIN_FEE_SAT);
        let mut candidates: Vec<&Coin> = st.coins.iter().filter(|c| !c.frozen).collect();
        candidates.sort_by_key(|c| c.amount);
        let mut coins = Vec::new();
        let mut total = Amount::ZERO;
        for c in candidates {
            if total.saturating_sub(fee) >= gap {
                break;
            }
            total = sum([total, c.amount].into_iter());
            coins.push(c.clone());
        }
        let arrives = total.saturating_sub(fee);
        if arrives < gap {
            return Err(FedimixError::InsufficientFunds {
                available: total,
                needed: with_fee(gap, fee),
            });
        }
        let preview = RefillPreview {
            id: st.fresh_id("refill"),
            amount: total,
            fee,
            coins,
            overshoot: arrives.saturating_sub(gap),
            device_name: device.name,
            expires_at: self.preview_expiry(),
        };
        let held = (preview.expires_at, preview.clone());
        st.refills.insert(preview.id.clone(), held);
        Ok(preview)
    }

    async fn set_coin_frozen(&self, outpoint: String, frozen: bool) -> Result<()> {
        {
            let mut st = self.state.lock().await;
            let coin = st
                .coins
                .iter_mut()
                .find(|c| c.outpoint == outpoint)
                .ok_or(FedimixError::UnknownItem(outpoint))?;
            coin.frozen = frozen;
        }
        self.emit(Event::SavingChanged);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::Duration;

    use super::*;
    use crate::backend::Backend;
    use crate::error::FedimixError;
    use crate::event::{Event, Notice};

    const ALL: [DemoScenario; 10] = [
        DemoScenario::Funded,
        DemoScenario::BelowTarget,
        DemoScenario::NoSaving,
        DemoScenario::FirstRun,
        DemoScenario::Offline,
        DemoScenario::ExitDeferred,
        DemoScenario::LongNames,
        DemoScenario::DeviceDisconnected,
        DemoScenario::PreviewExpiry,
        DemoScenario::DeviceRejects,
    ];

    fn btc(v: f64) -> Amount {
        Amount::from_sat((v * 1e8).round() as u64)
    }

    fn sum(amounts: impl Iterator<Item = Amount>) -> Amount {
        amounts.fold(Amount::ZERO, |a, b| a.checked_add(b).unwrap())
    }

    #[tokio::test]
    async fn funded_matches_mockup() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let s = b.saving_state().await.unwrap();
        assert_eq!(s.balance, btc(1.2034));
        assert_eq!(s.in_flight, btc(0.42));
        assert_eq!(s.device.unwrap().name, "Ledger Nano S+");
        let sp = b.spending_state().await.unwrap();
        assert_eq!(sp.balance, btc(0.47));
        assert_eq!(sp.for_spending, btc(0.05));
        assert_eq!(sp.moving, btc(0.42));
        assert_eq!(sp.target, btc(0.05));
        assert_eq!(sp.federation.unwrap().name, "Fedi Alpha");
    }

    #[tokio::test]
    async fn coins_sum_to_balance_all_scenarios() {
        for sc in ALL {
            let s = DemoBackend::new(sc).saving_state().await.unwrap();
            assert_eq!(sum(s.coins.iter().map(|c| c.amount)), s.balance, "{sc:?}");
        }
    }

    #[tokio::test]
    async fn queue_total_equals_moving_all_scenarios() {
        for sc in ALL {
            let b = DemoBackend::new(sc);
            let q = b.queue_state().await.unwrap();
            let sp = b.spending_state().await.unwrap();
            assert_eq!(q.total, sp.moving, "{sc:?}");
            assert_eq!(
                sum(q.items.iter().map(|i| i.denomination)),
                q.total,
                "{sc:?}"
            );
        }
    }

    #[tokio::test]
    async fn spending_parts_sum_to_balance_all_scenarios() {
        for sc in ALL {
            let sp = DemoBackend::new(sc).spending_state().await.unwrap();
            assert_eq!(
                sp.for_spending.checked_add(sp.moving).unwrap(),
                sp.balance,
                "{sc:?}"
            );
            let notes = sum(sp.notes.iter().map(|n| Amount {
                msat: n.denomination.msat * u64::from(n.count),
            }));
            assert_eq!(notes, sp.balance, "{sc:?}");
        }
    }

    const FAST: DemoTiming = DemoTiming {
        step: Duration::from_millis(1),
        preview_ttl: Duration::from_secs(600),
    };

    fn fast(scenario: DemoScenario) -> Arc<DemoBackend> {
        DemoBackend::with_timing(scenario, FAST)
    }

    /// Wait for the final state of `op`.
    async fn settled(events: &EventStream, op: Result<OperationId>) -> OperationState {
        let op = op.unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.next())
                .await
                .expect("timed out")
                .expect("stream ended");
            if let Event::Operation { id, state, .. } = event
                && id == op
                && state.is_final()
            {
                return state;
            }
        }
    }

    async fn exit(b: &Arc<DemoBackend>, item: &str) {
        let events = b.subscribe();
        let preview = b.prepare_exit(item.to_string()).await.unwrap();
        let op = b.exit_now(preview.id).await;
        assert_eq!(settled(&events, op).await, OperationState::Done);
    }

    #[tokio::test]
    async fn refill_below_target_matches_mockup() {
        let b = fast(DemoScenario::BelowTarget);
        let events = b.subscribe();
        let p = b.prepare_refill().await.unwrap();
        assert_eq!(p.amount, btc(0.06));
        assert_eq!(p.fee, Amount::from_sat(420));
        assert_eq!(p.overshoot, btc(0.0212).saturating_sub(p.fee));
        let amounts: Vec<_> = p.coins.iter().map(|c| c.amount).collect();
        assert_eq!(amounts, vec![btc(0.01), btc(0.05)]);
        let op = b.confirm_refill(p.id.clone()).await;
        settled(&events, op).await;
        assert!(b.confirm_refill(p.id).await.is_err());
    }

    #[tokio::test]
    async fn refill_at_target_rejected() {
        let b = DemoBackend::new(DemoScenario::Funded);
        assert_eq!(
            b.prepare_refill().await,
            Err(FedimixError::InvalidAmount("Spending is at target".into()))
        );
        let b = DemoBackend::new(DemoScenario::FirstRun);
        assert!(matches!(
            b.prepare_refill().await,
            Err(FedimixError::NotSetUp(_))
        ));
    }

    #[tokio::test]
    async fn exit_now_moves_item_and_emits() {
        let b = fast(DemoScenario::Funded);
        let events = b.subscribe();
        let q = b.queue_state().await.unwrap();
        let item = q.items[0].clone();
        exit(&b, &item.id).await;

        let q2 = b.queue_state().await.unwrap();
        assert_eq!(q2.items.len(), q.items.len() - 1);
        assert_eq!(q2.total, q.total.checked_sub(item.denomination).unwrap());
        let sp = b.spending_state().await.unwrap();
        assert_eq!(sp.moving, q2.total);
        let s = b.saving_state().await.unwrap();
        assert!(s.coins.iter().any(
            |c| matches!(c.tag, PrivacyTag::EarlyExit { .. }) && c.amount == item.denomination
        ));
        assert_eq!(sum(s.coins.iter().map(|c| c.amount)), s.balance);

        let mut seen = Vec::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(1), events.next())
                .await
                .expect("timed out")
                .expect("stream ended");
            let done = matches!(ev, Event::Notification(Notice::ExitLanded { .. }));
            seen.push(ev);
            if done {
                break;
            }
        }
        let added = seen
            .iter()
            .filter(|e| matches!(e, Event::HistoryItemAdded { .. }))
            .count();
        assert_eq!(added, 2);
        assert!(seen.contains(&Event::QueueChanged));
    }

    #[tokio::test]
    async fn exit_now_unknown_item_errors() {
        let b = fast(DemoScenario::Funded);
        assert!(matches!(
            b.exit_now("nope".into()).await,
            Err(FedimixError::UnknownItem(_))
        ));
    }

    fn send(wallet: WalletKind, dest: &str, amount: SendAmount) -> SendRequest {
        SendRequest {
            wallet,
            destination: dest.into(),
            amount,
            fee_rate_sat_vb: None,
        }
    }

    fn exact(v: f64) -> SendAmount {
        SendAmount::Exact(btc(v))
    }

    #[tokio::test]
    async fn send_rejects_garbage_destination() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let r = b
            .prepare_send(send(WalletKind::Saving, "garbage", exact(0.1)))
            .await;
        assert!(matches!(r, Err(FedimixError::InvalidDestination(_))));
    }

    #[tokio::test]
    async fn send_rejects_zero_amount() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let r = b
            .prepare_send(send(
                WalletKind::Saving,
                "bc1qabc",
                SendAmount::Exact(Amount::ZERO),
            ))
            .await;
        assert!(matches!(r, Err(FedimixError::InvalidAmount(_))));
        let r = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", SendAmount::FromInvoice))
            .await;
        assert!(matches!(r, Err(FedimixError::InvalidAmount(_))));
    }

    #[tokio::test]
    async fn send_rejects_above_balance() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let r = b
            .prepare_send(send(WalletKind::Spending, "bc1qabc", exact(0.47)))
            .await;
        assert!(matches!(r, Err(FedimixError::InsufficientFunds { .. })));
    }

    #[tokio::test]
    async fn send_previews_and_offline() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let p = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", exact(0.25)))
            .await
            .unwrap();
        assert_eq!(p.fee, Amount::from_sat(420));
        assert!(matches!(p.change, ChangeDestination::Spending { .. }));
        let ln = b
            .prepare_send(send(
                WalletKind::Spending,
                "lnbc1200u1x",
                SendAmount::FromInvoice,
            ))
            .await
            .unwrap();
        assert_eq!(ln.amount, btc(0.0012));
        assert_eq!(ln.fee, Amount::from_sat(12));
        let off = DemoBackend::new(DemoScenario::Offline);
        assert_eq!(
            off.prepare_send(send(
                WalletKind::Spending,
                "lnbc1200u1x",
                SendAmount::FromInvoice
            ))
            .await,
            Err(FedimixError::Offline)
        );
    }

    #[tokio::test]
    async fn saving_rejects_lightning() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let r = b
            .prepare_send(send(WalletKind::Saving, "lnbc1x", exact(0.1)))
            .await;
        assert!(matches!(r, Err(FedimixError::InvalidDestination(_))));
    }

    #[tokio::test]
    async fn destination_is_normalized_in_the_preview() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let p = b
            .prepare_send(send(
                WalletKind::Spending,
                "bitcoin:BC1QABC?amount=0.01",
                SendAmount::FromInvoice,
            ))
            .await
            .unwrap();
        assert_eq!(p.destination, "bc1qabc");
        assert_eq!(p.amount, btc(0.01));
    }

    #[tokio::test]
    async fn amount_mode_must_fit_the_destination() {
        let b = DemoBackend::new(DemoScenario::Funded);
        for (dest, amount) in [
            ("lnbc1x", SendAmount::FromInvoice),
            ("lnbc1200u1x", exact(0.5)),
            ("lnbc1200u1x", SendAmount::Max),
        ] {
            let r = b
                .prepare_send(send(WalletKind::Spending, dest, amount))
                .await;
            assert!(matches!(r, Err(FedimixError::InvalidAmount(_))), "{dest}");
        }
        // Same amount as the invoice is fine, and an amountless one takes any.
        for (dest, amount) in [("lnbc1200u1x", exact(0.0012)), ("lnbc1x", exact(0.01))] {
            assert!(
                b.prepare_send(send(WalletKind::Spending, dest, amount))
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn max_spends_everything_but_the_fee() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let p = b
            .prepare_send(send(WalletKind::Spending, "bc1qabc", SendAmount::Max))
            .await
            .unwrap();
        assert_eq!(p.amount.checked_add(p.fee), Some(btc(0.47)));
        let p = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", SendAmount::Max))
            .await
            .unwrap();
        assert_eq!(p.amount.checked_add(p.fee), Some(btc(1.2034)));
        assert_eq!(p.change, ChangeDestination::None);
        let empty = DemoBackend::new(DemoScenario::NoSaving);
        assert!(matches!(
            empty
                .prepare_send(send(WalletKind::Saving, "bc1qabc", SendAmount::Max))
                .await,
            Err(FedimixError::InsufficientFunds { .. })
        ));
    }

    #[tokio::test]
    async fn fee_rate_sets_the_saving_fee_only() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let mut r = send(WalletKind::Saving, "bc1qabc", exact(0.1));
        r.fee_rate_sat_vb = Some(10);
        assert_eq!(
            b.prepare_send(r.clone()).await.unwrap().fee,
            Amount::from_sat(1400)
        );
        r.wallet = WalletKind::Spending;
        assert_eq!(b.prepare_send(r).await.unwrap().fee, Amount::from_sat(420));
    }

    #[tokio::test]
    async fn confirm_send_twice_errors() {
        let b = fast(DemoScenario::Funded);
        let events = b.subscribe();
        let before = b.saving_state().await.unwrap();
        let p = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", exact(0.25)))
            .await
            .unwrap();
        let op = b.confirm_send(p.id.clone()).await;
        assert!(matches!(
            b.confirm_send(p.id).await,
            Err(FedimixError::UnknownItem(_))
        ));
        settled(&events, op).await;
        let after = b.saving_state().await.unwrap();
        assert!(after.balance < before.balance);
        assert_eq!(after.history.len(), before.history.len() + 1);
        assert_eq!(sum(after.coins.iter().map(|c| c.amount)), after.balance);
        // Notification arrives later; a second subscriber would also get it.
        let mut got = false;
        for _ in 0..10 {
            if let Ok(Some(Event::Notification(Notice::SendConfirmed { .. }))) =
                tokio::time::timeout(Duration::from_secs(3), events.next()).await
            {
                got = true;
                break;
            }
        }
        assert!(got);
    }

    async fn notes_sum(b: &DemoBackend) -> (Amount, Amount) {
        let sp = b.spending_state().await.unwrap();
        let notes = sum(sp.notes.iter().map(|n| Amount {
            msat: n.denomination.msat * u64::from(n.count),
        }));
        (notes, sp.balance)
    }

    #[tokio::test]
    async fn notes_sum_to_balance_after_balance_changes() {
        let b = fast(DemoScenario::BelowTarget);
        let events = b.subscribe();
        let p = b.prepare_refill().await.unwrap();
        settled(&events, b.confirm_refill(p.id).await).await;
        let (n, bal) = notes_sum(&b).await;
        assert_eq!(n, bal);
        let p = b
            .prepare_send(send(WalletKind::Spending, "bc1qabc", exact(0.01)))
            .await
            .unwrap();
        settled(&events, b.confirm_send(p.id).await).await;
        let (n, bal) = notes_sum(&b).await;
        assert_eq!(n, bal);
        let f = fast(DemoScenario::Funded);
        let events = f.subscribe();
        let p = f
            .prepare_send(send(WalletKind::Saving, "bc1qabc", exact(0.25)))
            .await
            .unwrap();
        settled(&events, f.confirm_send(p.id).await).await;
        let (n, bal) = notes_sum(&f).await;
        assert_eq!(n, bal);
    }

    #[tokio::test]
    async fn outpoints_unique_after_multi_input_send_and_exit() {
        let b = fast(DemoScenario::Funded);
        let events = b.subscribe();
        exit(&b, "queue-1").await;
        exit(&b, "queue-2").await;
        let p = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", exact(1.2)))
            .await
            .unwrap();
        assert!(p.inputs.len() > 1);
        settled(&events, b.confirm_send(p.id).await).await;
        exit(&b, "queue-3").await;
        let coins = b.saving_state().await.unwrap().coins;
        let mut ops: Vec<_> = coins.iter().map(|c| c.outpoint.clone()).collect();
        ops.sort();
        ops.dedup();
        assert_eq!(ops.len(), coins.len());
    }

    #[tokio::test]
    async fn every_subscriber_gets_every_event() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let e1 = b.subscribe();
        let e2 = b.subscribe();
        drop(b.subscribe());
        b.set_settings(Settings::default()).await.unwrap();
        b.set_settings(Settings::default()).await.unwrap();
        assert_eq!(e1.next().await, Some(Event::SettingsChanged));
        assert_eq!(e2.next().await, Some(Event::SettingsChanged));
        assert_eq!(e1.next().await, Some(Event::SettingsChanged));
        assert_eq!(e2.next().await, Some(Event::SettingsChanged));
    }

    #[tokio::test]
    async fn receive_honours_offline_and_reports_expiry() {
        let lightning = ReceiveRequest {
            wallet: WalletKind::Spending,
            method: ReceiveMethod::Lightning {
                amount: btc(0.001),
                description: "Coffee".into(),
                expiry_secs: 3600,
            },
        };
        let onchain = ReceiveRequest {
            wallet: WalletKind::Spending,
            method: ReceiveMethod::OnChain,
        };
        let saving = ReceiveRequest {
            wallet: WalletKind::Saving,
            method: ReceiveMethod::OnChain,
        };
        let off = DemoBackend::new(DemoScenario::Offline);
        assert_eq!(
            off.receive(lightning.clone()).await,
            Err(FedimixError::Offline)
        );
        assert_eq!(
            off.receive(onchain.clone()).await,
            Err(FedimixError::Offline)
        );
        assert!(
            off.receive(saving).await.is_ok(),
            "Saving needs no federation"
        );

        let on = DemoBackend::new(DemoScenario::Funded);
        let invoice = on.receive(lightning).await.unwrap();
        assert!(invoice.expires_at.unwrap() > now_secs() + 3000);
        assert_eq!(
            crate::parse_destination(&invoice.payload).unwrap().amount,
            Some(btc(0.001))
        );
        assert_eq!(on.receive(onchain).await.unwrap().expires_at, None);
    }

    #[tokio::test]
    async fn join_rejects_bad_invite() {
        let b = DemoBackend::new(DemoScenario::FirstRun);
        assert!(matches!(
            b.join_federation("nope".into()).await,
            Err(FedimixError::InvalidInvite(_))
        ));
    }

    #[tokio::test]
    async fn first_run_join_then_setup_saving() {
        let b = DemoBackend::new(DemoScenario::FirstRun);
        assert!(b.spending_state().await.unwrap().federation.is_none());
        b.join_federation("fed1abc".into()).await.unwrap();
        let sp = b.spending_state().await.unwrap();
        assert!(sp.federation.is_some());
        let dev = b.detect_devices().await.unwrap().remove(0);
        assert!(matches!(
            b.setup_saving("nope".into()).await,
            Err(FedimixError::UnknownItem(_))
        ));
        b.setup_saving(dev.id).await.unwrap();
        let sp = b.spending_state().await.unwrap();
        assert!(sp.saving_ready);
        assert!(b.saving_state().await.unwrap().device.is_some());
        assert!(!b.queue_state().await.unwrap().waiting_for_saving);
    }

    #[test]
    fn scenario_from_str_rejects_unknown() {
        assert_eq!(
            DemoScenario::from_str("below-target"),
            Ok(DemoScenario::BelowTarget)
        );
        assert_eq!(
            DemoScenario::from_str("first-run"),
            Ok(DemoScenario::FirstRun)
        );
        assert!(DemoScenario::from_str("bogus").is_err());
    }
}
