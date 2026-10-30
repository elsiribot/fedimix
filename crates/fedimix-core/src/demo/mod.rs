//! In-memory demo backend with mockup scenarios.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::api::*;
use crate::backend::Backend;
use crate::error::{FedimixError, Result};
use crate::event::{Event, EventStream, Notice};

mod fixtures;
use fixtures::{BASE_TS, btc};

const ONCHAIN_FEE_SAT: u64 = 420;
const LIGHTNING_FEE_SAT: u64 = 12;
const SMALL_CHANGE_SAT: u64 = 10_000;
const FIXED_INVOICE: f64 = 0.0012;
const NOTIFY_DELAY: Duration = Duration::from_secs(2);
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
            other => Err(format!(
                "unknown scenario '{other}' (expected funded, below-target, no-saving, first-run, offline)"
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
    sends: HashMap<String, SendPreview>,
    refills: HashMap<String, RefillPreview>,
    next_id: u64,
}

impl State {
    fn new(scenario: DemoScenario) -> Self {
        let f = fixtures::fixture(scenario);
        State {
            device: f.device,
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
}

fn sum(amounts: impl Iterator<Item = Amount>) -> Amount {
    amounts.fold(Amount::ZERO, |a, b| Amount {
        msat: a.msat.saturating_add(b.msat),
    })
}

pub struct DemoBackend {
    state: Mutex<State>,
    senders: Senders,
}

impl DemoBackend {
    pub fn new(scenario: DemoScenario) -> Arc<Self> {
        Arc::new(DemoBackend {
            state: Mutex::new(State::new(scenario)),
            senders: Arc::new(StdMutex::new(Vec::new())),
        })
    }

    fn emit(&self, event: Event) {
        broadcast(&self.senders, &event);
    }

    /// Emit `notice` after a short delay (no-op without a tokio runtime).
    fn notify_later(&self, notice: Notice) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let senders = Arc::clone(&self.senders);
        handle.spawn(async move {
            tokio::time::sleep(NOTIFY_DELAY).await;
            broadcast(&senders, &Event::Notification(notice));
        });
    }
}

fn valid_destination(dest: &str) -> bool {
    ["lnbc", "bc1", "tb1", "bcrt1"]
        .iter()
        .any(|p| dest.starts_with(p))
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
        Ok(fixtures::suggested())
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
        Ok(vec![fixtures::device()])
    }

    async fn setup_saving(&self, device: DeviceInfo) -> Result<()> {
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
        Ok(match request.method {
            ReceiveMethod::Lightning { amount } => {
                let payload = format!("lnbc{}n1pdemo0fedimix0invoice", amount.as_sat() * 10);
                ReceiveInfo {
                    uri: format!("lightning:{payload}"),
                    payload,
                }
            }
            ReceiveMethod::OnChain => {
                let payload = fixtures::address_for(0);
                ReceiveInfo {
                    uri: format!("bitcoin:{payload}"),
                    payload,
                }
            }
        })
    }

    async fn prepare_send(&self, request: SendRequest) -> Result<SendPreview> {
        let SendRequest {
            wallet,
            destination,
            amount,
        } = request;
        if !valid_destination(&destination) {
            return Err(FedimixError::InvalidDestination(destination));
        }
        let lightning = destination.starts_with("lnbc");
        let amount = match (amount, lightning) {
            (Some(a), _) => a,
            (None, true) => btc(FIXED_INVOICE),
            (None, false) => {
                return Err(FedimixError::InvalidAmount(
                    "amount required for on-chain sends".to_string(),
                ));
            }
        };
        if amount == Amount::ZERO {
            return Err(FedimixError::InvalidAmount(
                "amount must be above zero".to_string(),
            ));
        }
        let fee = Amount::from_sat(if lightning {
            LIGHTNING_FEE_SAT
        } else {
            ONCHAIN_FEE_SAT
        });
        let needed = amount.checked_add(fee).unwrap_or(Amount { msat: u64::MAX });

        let mut st = self.state.lock().await;
        let (inputs, change) = match wallet {
            WalletKind::Spending => {
                match &st.federation {
                    None => return Err(FedimixError::NotSetUp("no federation joined".into())),
                    Some(f) if !f.online => return Err(FedimixError::Offline),
                    Some(_) => {}
                }
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
                (vec![input], ChangeDestination::None)
            }
            WalletKind::Saving => {
                let balance = st.saving_balance();
                if needed > balance {
                    return Err(FedimixError::InsufficientFunds {
                        available: balance,
                        needed,
                    });
                }
                let mut candidates: Vec<&Coin> = st.coins.iter().filter(|c| !c.frozen).collect();
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
                if total < needed {
                    return Err(FedimixError::InsufficientFunds {
                        available: total,
                        needed,
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
                (inputs, change)
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
        };
        st.sends.insert(preview.id.clone(), preview.clone());
        Ok(preview)
    }

    async fn confirm_send(&self, preview_id: String) -> Result<()> {
        let mut events = Vec::new();
        let amount;
        {
            let mut st = self.state.lock().await;
            let p = st
                .sends
                .remove(&preview_id)
                .ok_or(FedimixError::UnknownItem(preview_id))?;
            amount = p.amount;
            let lightning = p.destination.starts_with("lnbc");
            match p.wallet {
                WalletKind::Spending => {
                    let total = p.amount.checked_add(p.fee).unwrap_or(p.amount);
                    st.spending_balance = st.spending_balance.saturating_sub(total);
                    st.for_spending = st.for_spending.saturating_sub(total);
                    let kind = if lightning {
                        HistoryKind::Paid
                    } else {
                        HistoryKind::Sent { change: None }
                    };
                    let item = st.history_item(kind, p.amount, false);
                    st.spending_history.insert(0, item.clone());
                    events.push(Event::BalanceChanged {
                        wallet: WalletKind::Spending,
                        balance: st.spending_balance,
                    });
                    events.push(Event::HistoryItemAdded {
                        wallet: WalletKind::Spending,
                        item,
                    });
                }
                WalletKind::Saving => {
                    st.coins
                        .retain(|c| !p.inputs.iter().any(|i| i.outpoint_or_note == c.outpoint));
                    let sent_change = match &p.change {
                        ChangeDestination::None => None,
                        ChangeDestination::Spending { amount } => {
                            st.spending_balance = sum([st.spending_balance, *amount].into_iter());
                            st.for_spending = sum([st.for_spending, *amount].into_iter());
                            events.push(Event::BalanceChanged {
                                wallet: WalletKind::Spending,
                                balance: st.spending_balance,
                            });
                            Some(SentChange {
                                amount: *amount,
                                to_spending: true,
                            })
                        }
                        ChangeDestination::SmallChange { amount } => {
                            let n = st.next_coin_index();
                            st.coins.push(Coin {
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
                    let item = st.history_item(
                        HistoryKind::Sent {
                            change: sent_change,
                        },
                        p.amount,
                        false,
                    );
                    st.saving_history.insert(0, item.clone());
                    events.push(Event::BalanceChanged {
                        wallet: WalletKind::Saving,
                        balance: st.saving_balance(),
                    });
                    events.push(Event::HistoryItemAdded {
                        wallet: WalletKind::Saving,
                        item,
                    });
                }
            }
        }
        for e in events {
            self.emit(e);
        }
        self.notify_later(Notice::SendConfirmed { amount });
        Ok(())
    }

    async fn prepare_refill(&self) -> Result<RefillPreview> {
        let mut st = self.state.lock().await;
        let device = st
            .device
            .clone()
            .ok_or_else(|| FedimixError::NotSetUp("no signing device".to_string()))?;
        let target = st.settings.spending_target;
        if st.for_spending >= target {
            return Err(FedimixError::InvalidAmount(
                "Spending is at target".to_string(),
            ));
        }
        let gap = target.saturating_sub(st.for_spending);
        let mut candidates: Vec<&Coin> = st.coins.iter().filter(|c| !c.frozen).collect();
        candidates.sort_by_key(|c| c.amount);
        let mut coins = Vec::new();
        let mut total = Amount::ZERO;
        for c in candidates {
            if total >= gap {
                break;
            }
            total = sum([total, c.amount].into_iter());
            coins.push(c.clone());
        }
        if total < gap {
            return Err(FedimixError::InsufficientFunds {
                available: total,
                needed: gap,
            });
        }
        let preview = RefillPreview {
            id: st.fresh_id("refill"),
            amount: total,
            fee: Amount::from_sat(ONCHAIN_FEE_SAT),
            coins,
            overshoot: total.saturating_sub(gap),
            device_name: device.name,
        };
        st.refills.insert(preview.id.clone(), preview.clone());
        Ok(preview)
    }

    async fn confirm_refill(&self, preview_id: String) -> Result<()> {
        let mut events = Vec::new();
        {
            let mut st = self.state.lock().await;
            let p = st
                .refills
                .remove(&preview_id)
                .ok_or(FedimixError::UnknownItem(preview_id))?;
            st.coins
                .retain(|c| !p.coins.iter().any(|r| r.outpoint == c.outpoint));
            let arrived = p.amount.saturating_sub(p.fee);
            st.spending_balance = sum([st.spending_balance, arrived].into_iter());
            st.for_spending = sum([st.for_spending, arrived].into_iter());
            let out = st.history_item(HistoryKind::Sent { change: None }, p.amount, false);
            let inc = st.history_item(HistoryKind::Deposit, arrived, true);
            st.saving_history.insert(0, out.clone());
            st.spending_history.insert(0, inc.clone());
            events.push(Event::BalanceChanged {
                wallet: WalletKind::Saving,
                balance: st.saving_balance(),
            });
            events.push(Event::HistoryItemAdded {
                wallet: WalletKind::Saving,
                item: out,
            });
            events.push(Event::BalanceChanged {
                wallet: WalletKind::Spending,
                balance: st.spending_balance,
            });
            events.push(Event::HistoryItemAdded {
                wallet: WalletKind::Spending,
                item: inc,
            });
        }
        for e in events {
            self.emit(e);
        }
        Ok(())
    }

    async fn exit_now(&self, item_id: String) -> Result<()> {
        let mut events = Vec::new();
        let denom;
        {
            let mut st = self.state.lock().await;
            let pos = st
                .queue
                .iter()
                .position(|i| i.id == item_id)
                .ok_or_else(|| FedimixError::UnknownItem(item_id.clone()))?;
            if st.device.is_none() {
                return Err(FedimixError::NotSetUp("no signing device".to_string()));
            }
            let item = st.queue.remove(pos);
            denom = item.denomination;
            st.moving = st.moving.saturating_sub(denom);
            st.spending_balance = st.spending_balance.saturating_sub(denom);
            st.in_flight = st.in_flight.saturating_sub(denom);
            let n = st.next_coin_index();
            let source_federation = st.federation.as_ref().map(|f| f.name.clone());
            st.coins.push(Coin {
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
            let spending_item = st.history_item(
                HistoryKind::ToSaving {
                    denomination: denom,
                },
                denom,
                false,
            );
            let saving_item = st.history_item(
                HistoryKind::FromSpending {
                    denomination: denom,
                },
                denom,
                true,
            );
            st.spending_history.insert(0, spending_item.clone());
            st.saving_history.insert(0, saving_item.clone());
            events.push(Event::QueueChanged);
            events.push(Event::BalanceChanged {
                wallet: WalletKind::Spending,
                balance: st.spending_balance,
            });
            events.push(Event::HistoryItemAdded {
                wallet: WalletKind::Spending,
                item: spending_item,
            });
            events.push(Event::BalanceChanged {
                wallet: WalletKind::Saving,
                balance: st.saving_balance(),
            });
            events.push(Event::HistoryItemAdded {
                wallet: WalletKind::Saving,
                item: saving_item,
            });
            events.push(Event::Notification(Notice::ExitLanded { amount: denom }));
        }
        for e in events {
            self.emit(e);
        }
        Ok(())
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

    const ALL: [DemoScenario; 5] = [
        DemoScenario::Funded,
        DemoScenario::BelowTarget,
        DemoScenario::NoSaving,
        DemoScenario::FirstRun,
        DemoScenario::Offline,
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

    #[tokio::test]
    async fn refill_below_target_matches_mockup() {
        let b = DemoBackend::new(DemoScenario::BelowTarget);
        let p = b.prepare_refill().await.unwrap();
        assert_eq!(p.amount, btc(0.06));
        assert_eq!(p.fee, Amount::from_sat(420));
        assert_eq!(p.overshoot, btc(0.0212));
        let amounts: Vec<_> = p.coins.iter().map(|c| c.amount).collect();
        assert_eq!(amounts, vec![btc(0.01), btc(0.05)]);
        b.confirm_refill(p.id.clone()).await.unwrap();
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
        let b = DemoBackend::new(DemoScenario::Funded);
        let events = b.subscribe();
        let q = b.queue_state().await.unwrap();
        let item = q.items[0].clone();
        b.exit_now(item.id.clone()).await.unwrap();

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
        let b = DemoBackend::new(DemoScenario::Funded);
        assert!(matches!(
            b.exit_now("nope".into()).await,
            Err(FedimixError::UnknownItem(_))
        ));
    }

    fn send(wallet: WalletKind, dest: &str, amount: Option<Amount>) -> SendRequest {
        SendRequest {
            wallet,
            destination: dest.into(),
            amount,
        }
    }

    #[tokio::test]
    async fn send_rejects_garbage_destination() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let r = b
            .prepare_send(send(WalletKind::Saving, "garbage", Some(btc(0.1))))
            .await;
        assert!(matches!(r, Err(FedimixError::InvalidDestination(_))));
    }

    #[tokio::test]
    async fn send_rejects_zero_amount() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let r = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", Some(Amount::ZERO)))
            .await;
        assert!(matches!(r, Err(FedimixError::InvalidAmount(_))));
        let r = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", None))
            .await;
        assert!(matches!(r, Err(FedimixError::InvalidAmount(_))));
    }

    #[tokio::test]
    async fn send_rejects_above_balance() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let r = b
            .prepare_send(send(WalletKind::Spending, "bc1qabc", Some(btc(0.47))))
            .await;
        assert!(matches!(r, Err(FedimixError::InsufficientFunds { .. })));
    }

    #[tokio::test]
    async fn send_previews_and_offline() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let p = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", Some(btc(0.25))))
            .await
            .unwrap();
        assert_eq!(p.fee, Amount::from_sat(420));
        assert!(matches!(p.change, ChangeDestination::Spending { .. }));
        let ln = b
            .prepare_send(send(WalletKind::Spending, "lnbc1x", None))
            .await
            .unwrap();
        assert_eq!(ln.amount, btc(0.0012));
        assert_eq!(ln.fee, Amount::from_sat(12));
        let off = DemoBackend::new(DemoScenario::Offline);
        assert_eq!(
            off.prepare_send(send(WalletKind::Spending, "lnbc1x", None))
                .await,
            Err(FedimixError::Offline)
        );
    }

    #[tokio::test]
    async fn confirm_send_twice_errors() {
        let b = DemoBackend::new(DemoScenario::Funded);
        let events = b.subscribe();
        let before = b.saving_state().await.unwrap();
        let p = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", Some(btc(0.25))))
            .await
            .unwrap();
        b.confirm_send(p.id.clone()).await.unwrap();
        assert!(matches!(
            b.confirm_send(p.id).await,
            Err(FedimixError::UnknownItem(_))
        ));
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
        let b = DemoBackend::new(DemoScenario::BelowTarget);
        let p = b.prepare_refill().await.unwrap();
        b.confirm_refill(p.id).await.unwrap();
        let (n, bal) = notes_sum(&b).await;
        assert_eq!(n, bal);
        let p = b
            .prepare_send(send(WalletKind::Spending, "bc1qabc", Some(btc(0.01))))
            .await
            .unwrap();
        b.confirm_send(p.id).await.unwrap();
        let (n, bal) = notes_sum(&b).await;
        assert_eq!(n, bal);
        let f = DemoBackend::new(DemoScenario::Funded);
        let p = f
            .prepare_send(send(WalletKind::Saving, "bc1qabc", Some(btc(0.25))))
            .await
            .unwrap();
        f.confirm_send(p.id).await.unwrap();
        let (n, bal) = notes_sum(&f).await;
        assert_eq!(n, bal);
    }

    #[tokio::test]
    async fn outpoints_unique_after_multi_input_send_and_exit() {
        let b = DemoBackend::new(DemoScenario::Funded);
        b.exit_now("queue-1".into()).await.unwrap();
        b.exit_now("queue-2".into()).await.unwrap();
        let p = b
            .prepare_send(send(WalletKind::Saving, "bc1qabc", Some(btc(1.2))))
            .await
            .unwrap();
        assert!(p.inputs.len() > 1);
        b.confirm_send(p.id).await.unwrap();
        b.exit_now("queue-3".into()).await.unwrap();
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
        b.setup_saving(dev).await.unwrap();
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
