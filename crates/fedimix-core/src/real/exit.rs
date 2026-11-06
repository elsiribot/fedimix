//! Exits: one queued denomination withdrawn from its federation to a fresh
//! address of Saving.
//!
//! An exit is made safe against crashes as a Spending send is: it is written
//! down under a fresh intent before anything is submitted, and that intent
//! travels in the withdrawal's metadata, so the operation log answers after a
//! crash whether it went out. One that never went out returns to the queue;
//! one whose fate is unknown never goes out again by itself. Each try gets an
//! address never given out before, and every address an exit was given keeps
//! its label for good, so a coin that ever arrives there is labelled.

use std::sync::Arc;

use super::Core;
#[cfg(feature = "testing")]
use super::held;
use super::receive::paid_fee;
#[cfg(feature = "testing")]
use super::send::SendHalt;
use super::send::{CAME_BACK, EXPIRED, PREVIEW_TTL, Reservation, failed_with, fresh_id};
use crate::api::*;
use crate::error::{FedimixError, Result};
use crate::event::{Event, Notice};
use crate::federation::{FederationClient, WatchUpdate, WithdrawQuote};
use crate::saving::{Labels, SavingWallet};
use crate::store::{ExitItem, ExitStart, ExitState, HistoryWrite, Watch, WatchKind};

const SET_UP_FIRST: &str = "Set up Saving first.";
const NEVER_SENT: &str = "This exit was never sent, so no money left Spending.";
const LOST_RECORD: &str = "Fedimix lost its record of this exit and cannot tell whether it was sent. If it was, it lands in Saving; if not, the money stays in Spending.";
const UNKNOWN_OUTCOME: &str = "Fedimix cannot tell whether this exit was sent. If it was, it lands in Saving; if not, the money stays in Spending.";
const UNEXPECTED_QUOTE: &str =
    "The federation quoted a different withdrawal than the exit asked for.";

/// An exit preview that can still be confirmed.
pub(super) struct HeldExit {
    item_id: String,
    expires_at: Timestamp,
    quote: WithdrawQuote,
}

fn is_exit(kind: WatchKind) -> bool {
    kind == WatchKind::Exit
}

impl Core {
    /// The item `item_id`, while it is queued: one on its way, landed or
    /// unknown cannot exit.
    fn queued_item(&self, item_id: &str) -> Result<ExitItem> {
        let items = self.store().exit_items(None)?;
        items
            .into_iter()
            .find(|item| item.id == item_id && item.state == ExitState::Queued)
            .ok_or_else(|| FedimixError::UnknownItem(item_id.to_string()))
    }

    /// No exit within the grace period after the federation came online on
    /// this connection, by hand or not. Offline is no grace period: it is
    /// refused as offline.
    fn check_grace(&self, federation: &str) -> Result<()> {
        let since = self
            .state()
            .live
            .get(federation)
            .and_then(|live| live.online_since);
        let Some(since) = since else {
            return Ok(());
        };
        let grace = self.store().settings()?.online_grace_secs;
        if (self.clock)() < since.saturating_add(grace) {
            return Err(FedimixError::GracePeriod);
        }
        Ok(())
    }

    /// Saving can take the exit, the federation can pay it, and its grace
    /// period is over. The client to pay it with.
    fn exit_client(&self, federation: &str) -> Result<Arc<dyn FederationClient>> {
        self.check_grace(federation)?;
        if !self.saving_usable() {
            return Err(FedimixError::NotSetUp(SET_UP_FIRST.into()));
        }
        self.ensure_open()?;
        self.connected(federation)
    }

    /// What exiting `item_id` now costs. The quote goes to the address Saving
    /// would hand out next, which it does not hand out: the exit reveals its
    /// own at confirmation. The fee guard, the minimum time and the spacing
    /// apply to automatic exits only.
    pub(super) async fn prepare_exit(&self, item_id: &str) -> Result<ExitPreview> {
        let item = self.queued_item(item_id)?;
        let federation = &item.federation_id;
        let client = self.exit_client(federation)?;
        let address = self.scanned_wallet()?.peek_receive_address()?;
        let exact = SendAmount::Exact(item.denomination);
        let quote = client.withdraw_quote(&address, exact).await?;
        if quote.amount != item.denomination {
            return Err(FedimixError::Internal(UNEXPECTED_QUOTE.into()));
        }
        let balance = client.balance().await?;
        let for_spending = self.exit_fee_room(federation, balance)?;
        if quote.fee > for_spending {
            return Err(FedimixError::InsufficientFunds {
                available: for_spending,
                needed: quote.fee,
            });
        }
        let preview = ExitPreview {
            id: fresh_id("preview"),
            item_id: item.id.clone(),
            fee: quote.fee,
            expires_at: (self.clock)() + PREVIEW_TTL,
        };
        let held = HeldExit {
            item_id: item.id,
            expires_at: preview.expires_at,
            quote,
        };
        self.state().exit_previews.insert(preview.id.clone(), held);
        Ok(preview)
    }

    /// The early exit the user asked for. The preview is gone whatever
    /// happens next.
    pub(super) async fn exit_now(self: &Arc<Self>, preview_id: &str) -> Result<OperationId> {
        let held = self.state().exit_previews.remove(preview_id);
        let held = held.ok_or_else(|| FedimixError::UnknownItem(preview_id.to_string()))?;
        if held.expires_at <= (self.clock)() {
            return Err(FedimixError::PreviewExpired);
        }
        let item = self.queued_item(&held.item_id)?;
        self.start_exit(&item, held.quote, true).await
    }

    /// Withdraws `item` to a fresh Saving address, at no more than `quote`'s
    /// fee. An `Err` means nothing was written and nothing paid. From `Ok` on,
    /// the outcome arrives as [`Event::Operation`] of kind `Exit`.
    ///
    /// The denomination and the fee are booked through [`Core::reserve`]
    /// against what the federation holds beyond its other queued items, so
    /// the fee comes from the part for spending, a send cannot take the
    /// exit's money, and the queue cannot grow onto it while it leaves.
    pub(super) async fn start_exit(
        self: &Arc<Self>,
        item: &ExitItem,
        quote: WithdrawQuote,
        early: bool,
    ) -> Result<OperationId> {
        let federation = item.federation_id.clone();
        let client = self.exit_client(&federation)?;
        let wallet = self.scanned_wallet()?;
        // Revealed, and written, before anything can pay it: never reused.
        let address = wallet.next_receive_address()?;
        let exact = SendAmount::Exact(item.denomination);
        let fresh = client.withdraw_quote(&address, exact).await?;
        if fresh.fee > quote.fee {
            return Err(FedimixError::PreviewExpired);
        }
        if fresh.amount != item.denomination || fresh.address != address {
            return Err(FedimixError::Internal(UNEXPECTED_QUOTE.into()));
        }
        let needed = fresh.amount.checked_add(fresh.fee).ok_or_else(|| {
            FedimixError::Internal("The federation's fee is too large to add up.".into())
        })?;
        let available = client.balance().await?;
        let estimate = self.item_estimate(&federation, item.origin, item.denomination)?;
        let intent = fresh_id("exit");
        let now = (self.clock)();
        let row = HistoryItem {
            id: intent.clone(),
            kind: HistoryKind::ToSaving {
                denomination: item.denomination,
            },
            amount: item.denomination,
            incoming: false,
            status: HistoryStatus::Pending,
            timestamp: now,
            txid: None,
            fee: Some(fresh.fee),
            session: None,
        };
        let watch = Watch {
            id: intent.clone(),
            federation_id: federation.clone(),
            kind: WatchKind::Exit,
            created_at: now,
        };
        let start = ExitStart {
            item_id: item.id.clone(),
            address,
            estimate,
            early,
            fee: fresh.fee,
            row: row.clone(),
            watch: watch.clone(),
        };
        // One hold of the store: the other items are read, the money booked
        // and the exit written, so no queue change falls in between.
        let reservation = {
            let store = self.store();
            let others = self.queued_beside(&store, &federation, &item.id)?;
            let reservation = self.reserve(
                &federation,
                &intent,
                needed,
                available.saturating_sub(others),
            )?;
            // After the reservation: `shutdown` closes the gate, then waits for those.
            self.ensure_open()?;
            store.begin_exit(&start)?;
            reservation
        };
        self.announce(&federation, &row, &HistoryWrite::Added);
        self.report_as(
            &intent,
            OperationKind::Exit,
            OperationState::Broadcasting,
            None,
        );
        self.events.emit(&Event::QueueChanged);
        self.events.emit(&Event::SavingChanged);
        // A task of its own: a caller that goes away must not cut a withdrawal off.
        let core = Arc::clone(self);
        tokio::spawn(async move {
            core.submit_exit(&*client, &watch, &fresh, reservation)
                .await;
        });
        Ok(intent)
    }

    /// The reservation outlives the writes here, as for a send.
    async fn submit_exit(
        &self,
        client: &dyn FederationClient,
        watch: &Watch,
        quote: &WithdrawQuote,
        reservation: Reservation,
    ) {
        #[cfg(feature = "testing")]
        if *held(&self.halt) == Some(SendHalt::BeforeSubmit) {
            return;
        }
        let submitted = client.withdraw(quote, &watch.id).await;
        #[cfg(feature = "testing")]
        if *held(&self.halt) == Some(SendHalt::AfterSubmit) {
            return;
        }
        let saved = match submitted {
            Ok(operation) => self.follow_operation(watch, &operation),
            // Fedimint commits the operation and its transaction together, so
            // an error means nothing left the wallet.
            Err(error) => self.fail_exit(watch, OperationState::Failed { error }),
        };
        // The row stays open, and the next connection settles it.
        if let Err(e) = saved {
            eprintln!("fedimix: saving what became of exit {}: {e}", watch.id);
        }
        drop(reservation);
    }

    /// The exit under `watch` took no money: its item goes back to the queue,
    /// keeping its origin, and its address keeps its label and is never used
    /// again. The item first, so a crash in between replays and changes
    /// nothing: it is matched by the intent, which the requeue clears.
    fn fail_exit(&self, watch: &Watch, state: OperationState) -> Result<()> {
        {
            let store = self.store();
            if let Some(item) = store.exit_by_intent(&watch.id)?
                && item.state == ExitState::Withdrawing
            {
                store.requeue_exit(&item.id)?;
            }
        }
        let failed = |row| HistoryItem {
            status: HistoryStatus::Failed,
            ..row
        };
        self.finish_row(watch, failed, state)?;
        self.events.emit(&Event::QueueChanged);
        self.events.emit(&Event::SavingChanged);
        Ok(())
    }

    /// `watch` is keyed on the intent, as the row is. Settled means the
    /// federation signed the withdrawal: the item stays on its way until its
    /// coin confirms in Saving.
    pub(super) fn apply_exit(&self, watch: &Watch, update: &WatchUpdate) -> Result<()> {
        let state = match update {
            WatchUpdate::Confirming { .. } => return Ok(()),
            WatchUpdate::Settled { fee, txid, .. } => {
                if let Some(txid) = txid {
                    let store = self.store();
                    if let Some(item) = store.exit_by_intent(&watch.id)? {
                        store.exit_settled(&item.id, txid)?;
                    }
                }
                let finished = |row: HistoryItem| HistoryItem {
                    status: HistoryStatus::Complete,
                    fee: paid_fee(*fee).or(row.fee),
                    txid: txid.clone().or(row.txid.clone()),
                    ..row
                };
                self.finish_row(watch, finished, OperationState::Done)?;
                self.events.emit(&Event::QueueChanged);
                return Ok(());
            }
            WatchUpdate::Refunded => failed_with(CAME_BACK),
            WatchUpdate::Expired => failed_with(EXPIRED),
            WatchUpdate::Failed { reason } => failed_with(reason),
            WatchUpdate::Unknown { reason } => {
                eprintln!("fedimix: exit {} may have left: {reason}", watch.id);
                return self.lose_exit(watch, UNKNOWN_OUTCOME);
            }
        };
        self.fail_exit(watch, state)
    }

    /// The exit under `watch` may have taken its money: its row ends with
    /// `sentence`, and its item stays on its way, never to go out again by
    /// itself. If it went out, its coin lands it; if not, the money is still
    /// in the balance, and the queue books it anew.
    fn lose_exit(&self, watch: &Watch, sentence: &str) -> Result<()> {
        let failed = |row| HistoryItem {
            status: HistoryStatus::Failed,
            ..row
        };
        self.finish_row(watch, failed, failed_with(sentence))?;
        self.events.emit(&Event::QueueChanged);
        self.events.emit(&Event::SavingChanged);
        Ok(())
    }

    /// The exits of `federation` with no operation that no call is working on.
    pub(super) fn unsent_exits(&self, federation: &str) -> Vec<Watch> {
        self.unsent_where(federation, is_exit)
    }

    /// Before a client database is rebuilt: an exit not yet settled, with an
    /// operation or not, can no longer be looked up or followed, so its row
    /// ends, and its item stays on its way rather than go out again.
    pub(super) fn finish_lost_exits(&self, federation: &str) -> Result<()> {
        let open = self.store().sends()?;
        let submitting = self.submitting.borrow().clone();
        for (watch, _) in open {
            if is_exit(watch.kind)
                && watch.federation_id == federation
                && !submitting.contains_key(&watch.id)
            {
                self.lose_exit(&watch, LOST_RECORD)?;
            }
        }
        Ok(())
    }

    /// After each Saving sync: an exit lands once its address holds a
    /// confirmed coin of exactly its denomination. The address decides, as
    /// only the federation knew it; the txid is a cross-check.
    pub(super) fn land_exits(&self, wallet: &SavingWallet) -> Result<()> {
        let items: Vec<ExitItem> = self
            .store()
            .exit_items(None)?
            .into_iter()
            .filter(|item| item.state == ExitState::Withdrawing && item.address.is_some())
            .collect();
        if items.is_empty() {
            return Ok(());
        }
        let labels = Labels::load(&self.store())?;
        let coins = wallet.coins(&labels);
        let mut landed = false;
        for item in items {
            let Some(coin) = coins.iter().find(|coin| {
                Some(coin.address.as_str()) == item.address.as_deref()
                    && coin.confirmed
                    && coin.amount == item.denomination
            }) else {
                continue;
            };
            let coin_txid = coin.outpoint.split(':').next();
            if let Some(txid) = item.txid.as_deref()
                && coin_txid != Some(txid)
            {
                eprintln!(
                    "fedimix: exit {} landed in {} but the federation named {txid}",
                    item.id, coin.outpoint
                );
            }
            self.store().exit_landed(&item.id)?;
            self.events.emit(&Event::Notification(Notice::ExitLanded {
                amount: item.denomination,
            }));
            landed = true;
        }
        if landed {
            self.events.emit(&Event::QueueChanged);
            self.events.emit(&Event::SavingChanged);
        }
        Ok(())
    }
}

/// Settles an exit a crash left with no operation. Never withdraws: found
/// resumes it, and not found returns its item to the queue. A failed lookup
/// leaves it for the next time the federation comes online.
pub(super) async fn resolve_exit(core: &Core, client: &dyn FederationClient, watch: &Watch) {
    let settled = match client.find_operation(&watch.id).await {
        Ok(Some(operation)) => core.follow_operation(watch, &operation),
        Ok(None) => core.fail_exit(watch, failed_with(NEVER_SENT)),
        Err(e) => Err(e),
    };
    if let Err(e) = settled {
        eprintln!("fedimix: looking up exit {}: {e}", watch.id);
    }
}
