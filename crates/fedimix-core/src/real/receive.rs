//! Receiving into Spending: invoices and deposits are written down before
//! anyone can pay them, followed to the end, and taken up again by each
//! connection from what the store still lists.
//!
//! Every step here that writes does so between awaits, so a task cut off at any
//! await leaves nothing half done. A crash between writing a history row and
//! finishing its watch replays the watch, which changes nothing.

use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::Core;
use super::send::{is_send, resolve_intent};
use crate::api::*;
use crate::error::{FedimixError, Result};
use crate::event::{Event, Notice};
use crate::federation::{DepositEvent, FederationClient, WatchUpdate};
use crate::store::{HistoryWrite, Watch, WatchKind};

/// How many deposit addresses of one federation stay watched while nobody pays
/// them. A v1 watch has no end of its own: its address never expires.
const MAX_DEPOSIT_WATCHES: usize = 100;

/// The position in a federation's deposit log, for a generation with no deposit
/// watches. Means nothing to another client database of that federation.
fn deposits_cursor(federation: &str) -> String {
    format!("deposits:{federation}")
}

const RESTORING: &str = "Fedimix is still restoring your wallet. Try again when it has finished.";

/// Not zero from the start of a federation's recovery until a client is usable.
fn recovery_mark(federation: &str) -> String {
    format!("recovering:{federation}")
}

/// A fee of zero is one that was not named.
pub(super) fn paid_fee(fee: Amount) -> Option<Amount> {
    (fee != Amount::ZERO).then_some(fee)
}

fn incoming(
    id: &str,
    kind: HistoryKind,
    amount: Amount,
    status: HistoryStatus,
    at: Timestamp,
) -> HistoryItem {
    HistoryItem {
        id: id.to_string(),
        kind,
        amount,
        incoming: true,
        status,
        timestamp: at,
        txid: None,
        fee: None,
        session: None,
    }
}

impl Core {
    /// The client of a federation that is connected now.
    pub(super) fn connected(&self, id: &str) -> Result<Arc<dyn FederationClient>> {
        let open = *self.gate.borrow() == super::Gate::Open;
        let state = self.state();
        if open && state.live.get(id).is_some_and(|live| live.recovering) {
            return Err(FedimixError::Internal(RESTORING.into()));
        }
        let live = state.live.get(id).filter(|live| live.online);
        live.and_then(|live| live.client.clone())
            .ok_or(FedimixError::Offline)
    }

    /// Hands a new watch to the running connection. With none, the next one
    /// finds it in the store.
    pub(super) fn start_watching(&self, watch: Watch) {
        let state = self.state();
        if let Some(tx) = state
            .live
            .get(&watch.federation_id)
            .and_then(|live| live.new_watches.as_ref())
        {
            let _ = tx.send(watch);
        }
    }

    pub(super) async fn receive(&self, request: ReceiveRequest) -> Result<ReceiveInfo> {
        if request.wallet == WalletKind::Saving {
            return self.saving_receive(&request.method);
        }
        let federation = self
            .store()
            .active_federation()?
            .ok_or_else(|| FedimixError::NotSetUp("Join a federation before receiving.".into()))?;
        self.ensure_open()?;
        let client = self.connected(&federation)?;
        match request.method {
            ReceiveMethod::Lightning {
                amount,
                description,
                expiry_secs,
            } => {
                // Before the call: the invoice's clock starts at the gateway.
                let asked_at = (self.clock)();
                let made = client
                    .lightning_invoice(amount, &description, expiry_secs)
                    .await?;
                self.add_invoice(&federation, &made.watch_id, amount)?;
                Ok(ReceiveInfo {
                    uri: format!("lightning:{}", made.invoice),
                    payload: made.invoice,
                    expires_at: Some(asked_at + u64::from(expiry_secs)),
                    min_deposit: None,
                    fee: made.fee,
                    address_repeats: false,
                })
            }
            ReceiveMethod::OnChain => {
                // First: a failure here must not use up an address.
                let min_deposit = client.min_deposit().await?;
                let (address, id) = client.deposit_address().await?;
                // A generation with no watch hands out one address at a time.
                let address_repeats = id.is_none();
                if let Some(id) = id {
                    self.add_deposit_watch(&federation, id)?;
                }
                Ok(ReceiveInfo {
                    uri: format!("bitcoin:{address}"),
                    payload: address,
                    expires_at: None,
                    min_deposit: Some(min_deposit),
                    fee: None,
                    address_repeats,
                })
            }
        }
    }

    fn add_invoice(&self, federation: &str, id: &str, amount: Amount) -> Result<()> {
        let now = (self.clock)();
        let item = incoming(
            id,
            HistoryKind::ReceivedLightning,
            amount,
            HistoryStatus::Pending,
            now,
        );
        let watch = Watch {
            id: id.to_string(),
            federation_id: federation.to_string(),
            kind: WatchKind::LnReceive,
            created_at: now,
        };
        self.store().add_tracked(&item, &watch)?;
        self.announce(federation, &item, &HistoryWrite::Added);
        self.start_watching(watch);
        Ok(())
    }

    pub(super) fn add_deposit_watch(&self, federation: &str, id: String) -> Result<()> {
        let watch = Watch {
            id,
            federation_id: federation.to_string(),
            kind: WatchKind::Deposit,
            created_at: (self.clock)(),
        };
        self.store()
            .add_deposit_watch(&watch, MAX_DEPOSIT_WATCHES)?;
        self.start_watching(watch);
        Ok(())
    }

    /// Tells subscribers what a write to Spending's history did. The `Notice`
    /// hangs on the store's own report of the transition, so a replay is silent.
    pub(super) fn announce(&self, federation: &str, item: &HistoryItem, write: &HistoryWrite) {
        let wallet = WalletKind::Spending;
        let item_event = match write {
            HistoryWrite::Unchanged => return,
            HistoryWrite::Added => Event::HistoryItemAdded {
                wallet,
                item: item.clone(),
            },
            HistoryWrite::Changed { .. } => Event::HistoryItemUpdated {
                wallet,
                item: item.clone(),
            },
        };
        let active = self.is_active(federation).unwrap_or(false);
        self.events.emit(&if active {
            item_event
        } else {
            Event::FederationChanged
        });
        self.wake_saving_for(item);
        let was_complete = *write
            == HistoryWrite::Changed {
                was: HistoryStatus::Complete,
            };
        if item.status == HistoryStatus::Complete && !was_complete {
            let amount = item.amount;
            self.events.emit(&Event::Notification(if item.incoming {
                Notice::DepositConfirmed { amount }
            } else {
                Notice::SendConfirmed { amount }
            }));
        }
    }

    /// A deposit of a payment from Saving that the federation sees in a block
    /// is on chain, so Saving's row of it may be final: Saving syncs now. Its
    /// idle sync can be minutes away, and Saving's row would say pending
    /// beside a credited deposit in Spending until then.
    fn wake_saving_for(&self, item: &HistoryItem) {
        let on_chain = matches!(
            item.status,
            HistoryStatus::Confirming { .. } | HistoryStatus::Complete
        );
        let Some(txid) = item.txid.as_deref().filter(|_| on_chain) else {
            return;
        };
        if item.kind != HistoryKind::Deposit {
            return;
        }
        match self.store().is_signed_txid(txid) {
            Ok(true) => self.saving_wake.notify_one(),
            Ok(false) => {}
            Err(e) => eprintln!("fedimix: looking up the deposit of {txid}: {e}"),
        }
    }

    pub(super) fn record(&self, federation: &str, item: &HistoryItem) -> Result<()> {
        let write = self
            .store()
            .upsert_history(WalletKind::Spending, Some(federation), item)?;
        self.announce(federation, item, &write);
        Ok(())
    }

    /// The session count to keep on a settled deposit. A replay keeps the first
    /// answer. A failed read leaves it unset, as a deposit must settle anyway.
    async fn deposit_session(
        &self,
        client: &dyn FederationClient,
        federation: &str,
        id: &str,
    ) -> Option<u64> {
        let row = self
            .store()
            .history_item(WalletKind::Spending, Some(federation), id);
        if let Ok(Some(HistoryItem {
            session: Some(session),
            ..
        })) = row
        {
            return Some(session);
        }
        client
            .session_count()
            .await
            .inspect_err(|e| eprintln!("fedimix: reading the session count of {federation}: {e}"))
            .ok()
    }

    /// Writes what `update` says about `watch`, and ends the watch if it is
    /// final. The row comes first, so a crash between the two replays.
    fn apply(&self, watch: &Watch, update: &WatchUpdate, session: Option<u64>) -> Result<()> {
        if is_send(watch.kind) {
            return self.apply_send(watch, update);
        }
        let federation = &watch.federation_id;
        let kind = match watch.kind {
            WatchKind::Deposit => HistoryKind::Deposit,
            _ => HistoryKind::ReceivedLightning,
        };
        let now = (self.clock)();
        let item = match update {
            WatchUpdate::Confirming {
                confirmations,
                required,
                amount,
                txid,
            } => {
                let status = HistoryStatus::Confirming {
                    confirmations: *confirmations,
                    required: *required,
                };
                Some(HistoryItem {
                    txid: txid.clone(),
                    ..incoming(&watch.id, kind, *amount, status, now)
                })
            }
            WatchUpdate::Settled { amount, fee, txid } => Some(HistoryItem {
                fee: paid_fee(*fee),
                txid: txid.clone(),
                session,
                ..incoming(&watch.id, kind, *amount, HistoryStatus::Complete, now)
            }),
            // These name no amount, so they can only update a row. A deposit
            // watch that has none saw no deposit.
            WatchUpdate::Expired | WatchUpdate::Refunded | WatchUpdate::Failed { .. } => {
                let row =
                    self.store()
                        .history_item(WalletKind::Spending, Some(federation), &watch.id)?;
                row.map(|row| HistoryItem {
                    status: match update {
                        WatchUpdate::Expired => HistoryStatus::Expired,
                        WatchUpdate::Refunded => HistoryStatus::Refunded,
                        _ => HistoryStatus::Failed,
                    },
                    ..row
                })
            }
        };
        if !update.is_final() {
            if let Some(item) = item {
                let write = self.store().upsert_progress(federation, &item)?;
                self.announce(federation, &item, &write);
            }
            return Ok(());
        }
        if let Some(item) = item {
            self.record(federation, &item)?;
        }
        self.store().finish_watch(&watch.id)
    }

    /// Opens the client of `id`, or recovers it if this device holds no data for
    /// it. A new client database starts a new deposit log, so the position in
    /// the old one is cleared before anything can be logged there.
    ///
    /// A recovery is marked in the store, because `open` resumes one that was
    /// cut off with no sign that it is one. The mark goes when a client is
    /// usable, so a recovery that keeps failing keeps being reported.
    pub(super) async fn connect(&self, id: &str) -> Result<Arc<dyn FederationClient>> {
        let mark = recovery_mark(id);
        if self.store().cursor(&mark)?.is_some_and(|set| set != 0) {
            self.set_recovering(id, true);
        }
        let client = match self.port.open(id, &self.seed).await {
            Err(FedimixError::UnknownItem(_)) => self.recover(id).await?,
            opened => opened?,
        };
        if let Err(e) = self.store().set_cursor(&mark, 0) {
            eprintln!("fedimix: clearing the recovery mark of federation {id}: {e}");
        }
        self.set_recovering(id, false);
        Ok(client)
    }

    fn set_recovering(&self, id: &str, recovering: bool) {
        let was = {
            let mut state = self.state();
            let live = state.live.entry(id.to_string()).or_default();
            std::mem::replace(&mut live.recovering, recovering)
        };
        if was != recovering {
            self.events.emit(&Event::FederationChanged);
        }
    }

    async fn recover(&self, id: &str) -> Result<Arc<dyn FederationClient>> {
        let invite = {
            let store = self.store();
            let rows = store.federations()?;
            let row = rows.iter().find(|row| row.id == id);
            let row = row.ok_or_else(|| FedimixError::UnknownItem(id.to_string()))?;
            store.set_cursor(&recovery_mark(id), 1)?;
            store.set_cursor(&deposits_cursor(id), 0)?;
            row.invite.clone()
        };
        self.set_recovering(id, true);
        self.finish_lost_intents(id)?;
        self.port.recover(&invite, &self.seed).await
    }

    /// Follows what the store still lists for this federation, once per
    /// connection. A stream that ends with no final update keeps its watch: the
    /// next connection comes back here.
    pub(super) fn resume_watches(
        self: &Arc<Self>,
        client: &Arc<dyn FederationClient>,
        federation: &str,
    ) -> Watches {
        // Before the store is read: a watch added after that read arrives here.
        let (tx, added) = mpsc::unbounded_channel();
        self.state()
            .live
            .entry(federation.to_string())
            .or_default()
            .new_watches = Some(tx);
        let mut watches = Watches {
            core: Arc::clone(self),
            client: Arc::clone(client),
            added,
            tasks: JoinSet::new(),
            started: HashSet::new(),
        };
        // Bound first: starting a watch reads the store too.
        let listed = self.store().watches();
        match listed {
            Ok(listed) => listed
                .into_iter()
                .filter(|watch| watch.federation_id == federation)
                .for_each(|watch| watches.start(watch)),
            Err(e) => eprintln!("fedimix: reading the watches of federation {federation}: {e}"),
        }
        let core = Arc::clone(self);
        match self.store().cursor(&deposits_cursor(federation)) {
            Ok(cursor) => {
                let events = client.deposits(cursor.unwrap_or(0));
                let federation = federation.to_string();
                let client = Arc::clone(client);
                watches.tasks.spawn(async move {
                    follow_deposits(&core, &*client, &federation, events).await;
                });
            }
            Err(e) => eprintln!("fedimix: reading the deposits of federation {federation}: {e}"),
        }
        watches
    }
}

/// The watches of one connection. Dropping this stops them.
pub(super) struct Watches {
    core: Arc<Core>,
    client: Arc<dyn FederationClient>,
    added: mpsc::UnboundedReceiver<Watch>,
    tasks: JoinSet<()>,
    /// A new watch can reach both the store's list and the channel.
    started: HashSet<String>,
}

impl Watches {
    fn start(&mut self, watch: Watch) {
        if self.started.contains(&watch.id) {
            return;
        }
        // A send with no operation yet has nothing to follow.
        let Some(followed) = self.core.followed(&watch) else {
            return;
        };
        self.started.insert(watch.id.clone());
        let updates = self.client.watch(followed);
        let core = Arc::clone(&self.core);
        let client = Arc::clone(&self.client);
        self.tasks
            .spawn(async move { follow(&core, &*client, &watch, updates).await });
    }

    /// Settles what a crash left of sends with no operation. Runs each time the
    /// federation comes online and on a slow timer while it stays online, so a
    /// lookup that failed, or an operation id that could not be saved, is tried
    /// again.
    pub(super) fn resolve_unsent(&mut self, federation: &str) {
        for watch in self.core.unsent(federation) {
            let core = Arc::clone(&self.core);
            let client = Arc::clone(&self.client);
            self.tasks
                .spawn(async move { resolve_intent(&core, &*client, &watch).await });
        }
    }

    /// Starts a watch that has arrived, or clears away one that ended. A panic
    /// ends only that watch, until the next connection.
    pub(super) async fn tend(&mut self) {
        tokio::select! {
            Some(watch) = self.added.recv() => self.start(watch),
            Some(ended) = self.tasks.join_next() => {
                if ended.is_err_and(|e| e.is_panic()) {
                    eprintln!("fedimix: a watch panicked; the next connection resumes it");
                }
            }
            else => std::future::pending().await,
        }
    }
}

/// A failed write leaves the watch unfinished, for the next connection.
async fn follow(
    core: &Core,
    client: &dyn FederationClient,
    watch: &Watch,
    mut updates: BoxStream<'static, WatchUpdate>,
) {
    while let Some(update) = updates.next().await {
        let settled = matches!(update, WatchUpdate::Settled { .. });
        let session = if settled && watch.kind == WatchKind::Deposit {
            core.deposit_session(client, &watch.federation_id, &watch.id)
                .await
        } else {
            None
        };
        if let Err(e) = core.apply(watch, &update, session) {
            eprintln!("fedimix: saving the progress of {}: {e}", watch.id);
            return;
        }
    }
}

/// The generation with no deposit watches reports deposits as a log. The row
/// comes before the cursor, so a crash between the two reads the event again.
async fn follow_deposits(
    core: &Core,
    client: &dyn FederationClient,
    federation: &str,
    mut events: BoxStream<'static, DepositEvent>,
) {
    while let Some(event) = events.next().await {
        let status = if event.confirmed {
            HistoryStatus::Complete
        } else {
            HistoryStatus::Pending
        };
        let session = if event.confirmed {
            core.deposit_session(client, federation, &event.id).await
        } else {
            None
        };
        let item = HistoryItem {
            fee: paid_fee(event.fee),
            txid: event.txid,
            session,
            ..incoming(
                &event.id,
                HistoryKind::Deposit,
                event.amount,
                status,
                (core.clock)(),
            )
        };
        let saved = core.record(federation, &item).and_then(|()| {
            core.store()
                .set_cursor(&deposits_cursor(federation), event.cursor)
        });
        if let Err(e) = saved {
            eprintln!("fedimix: saving a deposit of federation {federation}: {e}");
            return;
        }
    }
}
