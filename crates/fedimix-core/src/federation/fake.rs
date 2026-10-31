//! Scriptable stand-in for Fedimint, for tests of the logic above the port.
//! A client outlives shutdown, as a federation's data does, so a test restarts
//! by calling `open` again.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio::sync::watch;

use super::port::{
    DepositEvent, FederationClient, FederationPort, FederationPreview, LightningQuote, WatchUpdate,
    WithdrawQuote, WithdrawTerms,
};
use crate::api::{Amount, Network, NoteBucket, SendAmount};
use crate::error::{FedimixError, Result};
use crate::seed::Seed;
use crate::store::Watch;

/// An append-only log that streams follow, so a stream started late (a
/// restart) still sees everything.
struct Log<T> {
    items: watch::Sender<Vec<T>>,
}

impl<T: Clone + Send + Sync + 'static> Log<T> {
    fn new() -> Self {
        Self {
            items: watch::Sender::new(Vec::new()),
        }
    }

    fn push(&self, item: T) {
        self.items.send_modify(|items| items.push(item));
    }

    /// Follows from index `from`; ends when `alive` turns false.
    fn follow(&self, from: usize, alive: watch::Receiver<bool>) -> BoxStream<'static, T> {
        let items = self.items.subscribe();
        stream::unfold(
            (items, alive, from),
            |(mut items, mut alive, next)| async move {
                loop {
                    let item = items.borrow_and_update().get(next).cloned();
                    if let Some(item) = item {
                        return Some((item, (items, alive, next + 1)));
                    }
                    tokio::select! {
                        changed = items.changed() => changed.ok()?,
                        _ = alive.wait_for(|alive| !*alive) => return None,
                    }
                }
            },
        )
        .boxed()
    }
}

/// A scripted federation.
pub(crate) struct FakeClient {
    id: String,
    name: String,
    balance: watch::Sender<Amount>,
    online: watch::Sender<bool>,
    alive: watch::Sender<bool>,
    notes: Mutex<Vec<NoteBucket>>,
    min_deposit: Mutex<Result<Amount>>,
    /// `true` for the generation that reports deposits through a watch.
    deposit_watches: Mutex<bool>,
    lightning_fee: Mutex<Amount>,
    withdraw_fee: Mutex<Amount>,
    /// Next results of these calls; empty means the call works.
    invoice_errors: Mutex<VecDeque<FedimixError>>,
    quote_errors: Mutex<VecDeque<FedimixError>>,
    pay_errors: Mutex<VecDeque<FedimixError>>,
    withdraw_errors: Mutex<VecDeque<FedimixError>>,
    next_op: AtomicU32,
    /// Amounts behind invoices a test wants to pay, by invoice.
    invoice_amounts: Mutex<HashMap<String, Amount>>,
    paid: Mutex<Vec<String>>,
    withdrawn: Mutex<Vec<WithdrawQuote>>,
    watches: Mutex<HashMap<String, Arc<Log<WatchUpdate>>>>,
    deposits: Log<DepositEvent>,
    shutdowns: AtomicU32,
}

impl FakeClient {
    pub(crate) fn new(id: &str, name: &str) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_string(),
            name: name.to_string(),
            balance: watch::Sender::new(Amount::ZERO),
            online: watch::Sender::new(true),
            alive: watch::Sender::new(true),
            notes: Mutex::new(Vec::new()),
            min_deposit: Mutex::new(Ok(Amount::from_sat(1_000))),
            deposit_watches: Mutex::new(true),
            lightning_fee: Mutex::new(Amount::from_sat(10)),
            withdraw_fee: Mutex::new(Amount::from_sat(300)),
            invoice_errors: Mutex::default(),
            quote_errors: Mutex::default(),
            pay_errors: Mutex::default(),
            withdraw_errors: Mutex::default(),
            next_op: AtomicU32::new(1),
            invoice_amounts: Mutex::default(),
            paid: Mutex::default(),
            withdrawn: Mutex::default(),
            watches: Mutex::default(),
            deposits: Log::new(),
            shutdowns: AtomicU32::new(0),
        })
    }

    pub(crate) fn set_balance(&self, balance: Amount) {
        self.balance.send_replace(balance);
    }

    pub(crate) fn set_online(&self, online: bool) {
        self.online.send_replace(online);
    }

    pub(crate) fn set_notes(&self, notes: Vec<NoteBucket>) {
        *self.notes.lock().unwrap() = notes;
    }

    pub(crate) fn set_min_deposit(&self, min_deposit: Result<Amount>) {
        *self.min_deposit.lock().unwrap() = min_deposit;
    }

    /// `false` acts like the generation with `deposits()` and no watch id.
    pub(crate) fn set_deposit_watches(&self, on: bool) {
        *self.deposit_watches.lock().unwrap() = on;
    }

    pub(crate) fn set_lightning_fee(&self, fee: Amount) {
        *self.lightning_fee.lock().unwrap() = fee;
    }

    pub(crate) fn set_withdraw_fee(&self, fee: Amount) {
        *self.withdraw_fee.lock().unwrap() = fee;
    }

    /// Makes `invoice` payable for `amount`.
    pub(crate) fn add_invoice(&self, invoice: &str, amount: Amount) {
        self.invoice_amounts
            .lock()
            .unwrap()
            .insert(invoice.to_string(), amount);
    }

    pub(crate) fn fail_next_invoice(&self, error: FedimixError) {
        self.invoice_errors.lock().unwrap().push_back(error);
    }

    /// Errors the next `lightning_quote` or `withdraw_quote`.
    pub(crate) fn fail_next_quote(&self, error: FedimixError) {
        self.quote_errors.lock().unwrap().push_back(error);
    }

    pub(crate) fn fail_next_pay(&self, error: FedimixError) {
        self.pay_errors.lock().unwrap().push_back(error);
    }

    pub(crate) fn fail_next_withdraw(&self, error: FedimixError) {
        self.withdraw_errors.lock().unwrap().push_back(error);
    }

    /// Appends to what `watch` yields for this id, also for a later watch.
    pub(crate) fn push_update(&self, id: &str, update: WatchUpdate) {
        self.log_of(id).push(update);
    }

    pub(crate) fn push_deposit(&self, event: DepositEvent) {
        self.deposits.push(event);
    }

    /// Invoices paid, in order.
    pub(crate) fn paid(&self) -> Vec<String> {
        self.paid.lock().unwrap().clone()
    }

    pub(crate) fn withdrawn(&self) -> Vec<WithdrawQuote> {
        self.withdrawn.lock().unwrap().clone()
    }

    pub(crate) fn shutdowns(&self) -> u32 {
        self.shutdowns.load(Ordering::SeqCst)
    }

    fn log_of(&self, id: &str) -> Arc<Log<WatchUpdate>> {
        let mut watches = self.watches.lock().unwrap();
        Arc::clone(
            watches
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(Log::new())),
        )
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.next_op.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}-{n}")
    }

    /// Takes `amount` plus `fee` from the balance, or refuses.
    fn debit(&self, amount: Amount, fee: Amount) -> Result<()> {
        let needed = amount.checked_add(fee).expect("test amounts fit");
        let available = *self.balance.borrow();
        let Some(left) = available.checked_sub(needed) else {
            return Err(FedimixError::InsufficientFunds { available, needed });
        };
        self.balance.send_replace(left);
        Ok(())
    }

    fn take_error(errors: &Mutex<VecDeque<FedimixError>>) -> Result<()> {
        errors.lock().unwrap().pop_front().map_or(Ok(()), Err)
    }
}

#[async_trait]
impl FederationClient for FakeClient {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    async fn balance(&self) -> Result<Amount> {
        Ok(*self.balance.borrow())
    }

    fn balance_changes(&self) -> BoxStream<'static, Amount> {
        changes(self.balance.subscribe(), self.alive.subscribe())
    }

    fn online_changes(&self) -> BoxStream<'static, bool> {
        changes(self.online.subscribe(), self.alive.subscribe())
    }

    async fn notes(&self) -> Result<Vec<NoteBucket>> {
        Ok(self.notes.lock().unwrap().clone())
    }

    async fn min_deposit(&self) -> Result<Amount> {
        self.min_deposit.lock().unwrap().clone()
    }

    async fn lightning_invoice(
        &self,
        _amount: Amount,
        _description: &str,
        _expiry_secs: u32,
    ) -> Result<(String, String)> {
        Self::take_error(&self.invoice_errors)?;
        let id = self.next_id("ln-receive");
        Ok((format!("lnbcrt-{id}"), id))
    }

    async fn deposit_address(&self) -> Result<(String, Option<String>)> {
        let id = self.next_id("deposit");
        let watch = (*self.deposit_watches.lock().unwrap()).then(|| id.clone());
        Ok((format!("bcrt1-{id}"), watch))
    }

    async fn lightning_quote(&self, invoice: &str) -> Result<LightningQuote> {
        Self::take_error(&self.quote_errors)?;
        let amounts = self.invoice_amounts.lock().unwrap();
        let amount = *amounts.get(invoice).ok_or_else(|| {
            FedimixError::InvalidDestination("That is not a valid Lightning invoice.".into())
        })?;
        Ok(LightningQuote {
            amount,
            fee: *self.lightning_fee.lock().unwrap(),
        })
    }

    async fn pay_invoice(&self, invoice: &str) -> Result<String> {
        Self::take_error(&self.pay_errors)?;
        let quote = self.lightning_quote(invoice).await?;
        self.debit(quote.amount, quote.fee)?;
        self.paid.lock().unwrap().push(invoice.to_string());
        Ok(self.next_id("ln-send"))
    }

    async fn withdraw_quote(&self, address: &str, amount: SendAmount) -> Result<WithdrawQuote> {
        Self::take_error(&self.quote_errors)?;
        let fee = *self.withdraw_fee.lock().unwrap();
        let amount = match amount {
            SendAmount::Exact(amount) => amount,
            SendAmount::Max => self.balance.borrow().saturating_sub(fee),
            SendAmount::FromInvoice => {
                return Err(FedimixError::InvalidAmount(
                    "This address does not carry an amount.".into(),
                ));
            }
        };
        Ok(WithdrawQuote {
            address: address.to_string(),
            amount,
            fee,
            terms: WithdrawTerms::V2 { miner_fee_sat: 0 },
        })
    }

    async fn withdraw(&self, quote: &WithdrawQuote) -> Result<String> {
        Self::take_error(&self.withdraw_errors)?;
        self.debit(quote.amount, quote.fee)?;
        self.withdrawn.lock().unwrap().push(quote.clone());
        Ok(self.next_id("withdraw"))
    }

    /// Ends after a final update, as the real one does.
    fn watch(&self, watch: Watch) -> BoxStream<'static, WatchUpdate> {
        self.log_of(&watch.id)
            .follow(0, self.alive.subscribe())
            .scan(false, |ended, update| {
                let item = (!*ended).then(|| {
                    *ended = update.is_final();
                    update
                });
                std::future::ready(item)
            })
            .boxed()
    }

    /// `cursor` counts events, so an event's own `cursor` is its index + 1.
    fn deposits(&self, cursor: u64) -> BoxStream<'static, DepositEvent> {
        let from = usize::try_from(cursor).expect("test cursors fit");
        self.deposits.follow(from, self.alive.subscribe())
    }

    async fn shutdown(self: Arc<Self>) -> Result<()> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        self.alive.send_replace(false);
        Ok(())
    }
}

/// The current value, then each change, until `alive` turns false.
fn changes<T>(values: watch::Receiver<T>, alive: watch::Receiver<bool>) -> BoxStream<'static, T>
where
    T: Copy + Send + Sync + 'static,
{
    stream::unfold(
        (values, alive, true),
        |(mut values, mut alive, first)| async move {
            if !first {
                tokio::select! {
                    changed = values.changed() => changed.ok()?,
                    _ = alive.wait_for(|alive| !*alive) => return None,
                }
            }
            let value = *values.borrow_and_update();
            Some((value, (values, alive, false)))
        },
    )
    .boxed()
}

/// A port over scripted federations, keyed by invite.
#[derive(Default)]
pub(crate) struct FakePort {
    invites: Mutex<HashMap<String, (FederationPreview, Arc<FakeClient>)>>,
    /// While set, every call reports `Offline`.
    unreachable: Mutex<bool>,
    /// Calls that would have opened a connection.
    connects: AtomicU32,
}

impl FakePort {
    /// Adds a federation on `network` with Lightning; returns its client.
    pub(crate) fn add_federation(
        &self,
        invite: &str,
        id: &str,
        network: Network,
    ) -> Arc<FakeClient> {
        let name = format!("Federation {id}");
        let client = FakeClient::new(id, &name);
        let preview = FederationPreview {
            id: id.to_string(),
            name,
            network: Some(network),
            has_lightning: true,
        };
        self.invites
            .lock()
            .unwrap()
            .insert(invite.to_string(), (preview, Arc::clone(&client)));
        client
    }

    pub(crate) fn set_unreachable(&self, unreachable: bool) {
        *self.unreachable.lock().unwrap() = unreachable;
    }

    pub(crate) fn connects(&self) -> u32 {
        self.connects.load(Ordering::SeqCst)
    }

    fn connect(&self) -> Result<()> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        if *self.unreachable.lock().unwrap() {
            return Err(FedimixError::Offline);
        }
        Ok(())
    }

    fn by_invite(&self, invite: &str) -> Result<(FederationPreview, Arc<FakeClient>)> {
        self.invites
            .lock()
            .unwrap()
            .get(invite)
            .cloned()
            .ok_or_else(|| FedimixError::InvalidInvite("That is not a valid invite code.".into()))
    }
}

/// A restarted client sees everything but its stopped streams come back.
fn reopened(client: Arc<FakeClient>) -> Arc<dyn FederationClient> {
    client.alive.send_replace(true);
    client
}

#[async_trait]
impl FederationPort for FakePort {
    async fn preview(&self, invite: &str) -> Result<FederationPreview> {
        self.connect()?;
        Ok(self.by_invite(invite)?.0)
    }

    async fn join(&self, invite: &str, _root: &Seed) -> Result<Arc<dyn FederationClient>> {
        self.connect()?;
        Ok(reopened(self.by_invite(invite)?.1))
    }

    async fn open(&self, id: &str, _root: &Seed) -> Result<Arc<dyn FederationClient>> {
        self.connect()?;
        let invites = self.invites.lock().unwrap();
        let (_, client) = invites
            .values()
            .find(|(preview, _)| preview.id == id)
            .ok_or_else(|| FedimixError::UnknownItem(id.to_string()))?;
        Ok(reopened(Arc::clone(client)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::WatchKind;

    fn watch_of(id: &str) -> Watch {
        Watch {
            id: id.to_string(),
            federation_id: "fed".into(),
            kind: WatchKind::LnSend,
            created_at: 0,
        }
    }

    #[tokio::test]
    async fn a_watch_ends_after_its_final_update_also_when_started_late() {
        let client = FakeClient::new("fed", "Fed");
        client.push_update(
            "op",
            WatchUpdate::Confirming {
                confirmations: 1,
                required: 2,
                amount: Amount::from_sat(5),
            },
        );
        client.push_update("op", WatchUpdate::Expired);
        client.push_update("op", WatchUpdate::Refunded);
        let updates: Vec<_> = client.watch(watch_of("op")).collect().await;
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[1], WatchUpdate::Expired);
    }

    #[tokio::test]
    async fn balance_stream_gives_current_then_changes_and_ends_on_shutdown() {
        let client = FakeClient::new("fed", "Fed");
        client.set_balance(Amount::from_sat(1));
        let mut balances = client.balance_changes();
        assert_eq!(balances.next().await, Some(Amount::from_sat(1)));
        client.set_balance(Amount::from_sat(2));
        assert_eq!(balances.next().await, Some(Amount::from_sat(2)));
        Arc::clone(&client).shutdown().await.unwrap();
        assert_eq!(balances.next().await, None);
        assert_eq!(client.shutdowns(), 1);
    }

    #[tokio::test]
    async fn pay_debits_and_refuses_without_funds() {
        let client = FakeClient::new("fed", "Fed");
        client.add_invoice("inv", Amount::from_sat(100));
        client.set_lightning_fee(Amount::from_sat(1));
        client.set_balance(Amount::from_sat(150));
        client.pay_invoice("inv").await.unwrap();
        assert_eq!(client.balance().await.unwrap(), Amount::from_sat(49));
        assert!(matches!(
            client.pay_invoice("inv").await,
            Err(FedimixError::InsufficientFunds { .. })
        ));
        assert_eq!(client.paid(), ["inv"]);
    }

    #[tokio::test]
    async fn deposits_resume_after_a_cursor() {
        let client = FakeClient::new("fed", "Fed");
        client.set_deposit_watches(false);
        assert_eq!(client.deposit_address().await.unwrap().1, None);
        for cursor in 1..=2 {
            client.push_deposit(DepositEvent {
                cursor,
                id: format!("d{cursor}"),
                amount: Amount::from_sat(1),
                fee: Amount::ZERO,
                txid: None,
                confirmed: true,
            });
        }
        let first = client.deposits(1).next().await.unwrap();
        assert_eq!(first.id, "d2");
    }

    #[tokio::test]
    async fn an_unreachable_port_counts_the_attempt() {
        let port = FakePort::default();
        port.add_federation("invite", "fed", Network::Regtest);
        port.set_unreachable(true);
        assert_eq!(
            port.preview("invite").await.err(),
            Some(FedimixError::Offline)
        );
        port.set_unreachable(false);
        assert_eq!(port.preview("invite").await.unwrap().id, "fed");
        assert_eq!(port.connects(), 2);
        assert_eq!(
            port.preview("other").await.err(),
            Some(FedimixError::InvalidInvite(
                "That is not a valid invite code.".into()
            ))
        );
    }
}
