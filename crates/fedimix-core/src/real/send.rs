//! Sending from Spending, with crash safety by intent.
//!
//! The operation id exists only once fedimint has the payment, so a send is
//! written down first under a fresh intent id, which is also its history row and
//! its [`OperationId`]. The intent travels in the operation's metadata, and
//! fedimint commits that metadata and the transaction in one database write.
//! So after a crash the operation log answers whether the payment was sent:
//! found resumes it, not found was never submitted. Nothing here pays on its
//! own: the only `pay_invoice` and `withdraw` are behind a consumed preview.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::Core;
#[cfg(feature = "testing")]
use super::held;
use super::receive::paid_fee;
use crate::api::*;
use crate::destination::{DestinationKind, parse_destination, requested_amount, validate_address};
use crate::error::{FedimixError, Result};
use crate::event::Event;
use crate::federation::{FederationClient, LightningQuote, WatchUpdate, WithdrawQuote};
use crate::store::{HistoryWrite, Watch, WatchKind};

/// How long a preview can be confirmed, as in the demo.
pub(super) const PREVIEW_TTL: Timestamp = 600;
/// How long `shutdown` waits for submissions, after closing clients took up to
/// their own 10 s: up to 25 s in all, inside GTK's cap of 30 s.
const SUBMISSION_GRACE: Duration = Duration::from_secs(15);
const CAME_BACK: &str = "The payment failed and the money came back.";
const NEVER_SENT: &str = "This payment was never sent, so no money left your wallet.";
const LOST_RECORD: &str = "Fedimix lost its record of this payment and cannot tell whether it was sent. Check your balance before paying again.";
const EXPIRED: &str = "The payment expired before it went through.";
const NO_AMOUNT: &str = "This invoice has no amount, so it cannot be paid.";

/// Where a test stops a send, as a crash there would.
#[cfg(feature = "testing")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendHalt {
    /// The row is written and nothing is submitted.
    BeforeSubmit,
    /// The payment is submitted and its operation id is not written.
    AfterSubmit,
}

/// Every later send on `backend` stops at `halt`.
#[cfg(feature = "testing")]
pub fn halt_sends(backend: &super::RealBackend, halt: SendHalt) {
    *held(&backend.core.halt) = Some(halt);
}

pub(super) fn is_send(kind: WatchKind) -> bool {
    matches!(kind, WatchKind::LnSend | WatchKind::Withdraw)
}

/// The whole quote: paying takes it back.
#[derive(Debug, Clone)]
enum Quote {
    Lightning(LightningQuote),
    OnChain(WithdrawQuote),
}

impl Quote {
    fn amount(&self) -> Amount {
        match self {
            Self::Lightning(quote) => quote.amount,
            Self::OnChain(quote) => quote.amount,
        }
    }

    fn fee(&self) -> Amount {
        match self {
            Self::Lightning(quote) => quote.fee,
            Self::OnChain(quote) => quote.fee,
        }
    }

    fn total(&self) -> Result<Amount> {
        self.amount().checked_add(self.fee()).ok_or_else(|| {
            FedimixError::Internal("The federation's fee is too large to add up.".into())
        })
    }
}

/// A preview that can still be confirmed.
pub(super) struct HeldSend {
    federation: String,
    expires_at: Timestamp,
    quote: Quote,
}

/// What each send being submitted will take from its federation.
pub(super) type Submitting = HashMap<String, (String, Amount)>;

/// Keeps a send in [`Core::submitting`], also through a panic.
pub(super) struct Reservation {
    core: Arc<Core>,
    intent: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.core.submitting.send_modify(|submitting| {
            submitting.remove(&self.intent);
        });
    }
}

pub(super) fn fresh_id(prefix: &str) -> String {
    format!("{prefix}-{:032x}", rand::random::<u128>())
}

fn insufficient(available: Amount, needed: Amount) -> FedimixError {
    FedimixError::InsufficientFunds { available, needed }
}

pub(super) fn failed_with(sentence: &str) -> OperationState {
    OperationState::Failed {
        error: FedimixError::Internal(sentence.to_string()),
    }
}

/// Quotes once more, because the fee moves: wallets reject old fee terms, and
/// the federation's fee follows the notes held at the moment of paying. Never
/// more than was shown.
async fn fresh_quote(client: &dyn FederationClient, quote: Quote) -> Result<Quote> {
    let fresh = match &quote {
        Quote::Lightning(shown) => Quote::Lightning(client.lightning_quote(&shown.invoice).await?),
        Quote::OnChain(shown) => {
            let exact = SendAmount::Exact(shown.amount);
            Quote::OnChain(client.withdraw_quote(&shown.address, exact).await?)
        }
    };
    if fresh.fee() > quote.fee() {
        return Err(FedimixError::PreviewExpired);
    }
    Ok(fresh)
}

impl Core {
    pub(super) async fn prepare_send(&self, request: SendRequest) -> Result<SendPreview> {
        if request.wallet == WalletKind::Saving {
            return self.prepare_saving_send(request).await;
        }
        let destination = parse_destination(&request.destination)?;
        let federation = self
            .store()
            .active_federation()?
            .ok_or_else(|| FedimixError::NotSetUp("Join a federation before sending.".into()))?;
        self.ensure_open()?;
        let client = self.connected(&federation)?;
        let quote = match destination.kind {
            // Its network and expiry are checked by the quote, which can read it.
            DestinationKind::Lightning => {
                let carried = destination
                    .amount
                    .ok_or_else(|| FedimixError::InvalidAmount(NO_AMOUNT.into()))?;
                requested_amount(request.amount, Some(carried))?;
                Quote::Lightning(client.lightning_quote(&destination.normalized).await?)
            }
            DestinationKind::OnChain => {
                validate_address(&destination.normalized, self.config.network)?;
                let amount = requested_amount(request.amount, destination.amount)?;
                let address = &destination.normalized;
                Quote::OnChain(client.withdraw_quote(address, amount).await?)
            }
        };
        let available = client.balance().await?;
        let needed = quote.total()?;
        if needed > available || quote.amount() == Amount::ZERO {
            return Err(insufficient(available, needed));
        }
        let preview = SendPreview {
            id: fresh_id("preview"),
            wallet: WalletKind::Spending,
            destination: destination.normalized,
            amount: quote.amount(),
            fee: quote.fee(),
            federation: Some(federation.clone()),
            inputs: Vec::new(),
            change: ChangeDestination::None,
            notes: Vec::new(),
            expires_at: (self.clock)() + PREVIEW_TTL,
        };
        let held = HeldSend {
            federation,
            expires_at: preview.expires_at,
            quote,
        };
        self.state().previews.insert(preview.id.clone(), held);
        Ok(preview)
    }

    /// Removes the preview, so it is gone whatever happens next: a failure
    /// before submission restores nothing, and the user previews again.
    fn consume(&self, id: &str) -> Result<HeldSend> {
        let held = self.state().previews.remove(id);
        let held = held.ok_or_else(|| FedimixError::UnknownItem(id.to_string()))?;
        if held.expires_at <= (self.clock)() {
            return Err(FedimixError::PreviewExpired);
        }
        Ok(held)
    }

    /// Books `needed` against what `available` leaves after the sends still being
    /// submitted, whose debit the balance may not show yet. One that has just
    /// debited can count twice for a moment: that refuses, and pays nothing.
    pub(super) fn reserve(
        self: &Arc<Self>,
        federation: &str,
        intent: &str,
        needed: Amount,
        available: Amount,
    ) -> Result<Reservation> {
        let mut outcome = Ok(());
        self.submitting.send_if_modified(|submitting| {
            let free = submitting
                .values()
                .filter(|(of, _)| of == federation)
                .fold(available, |free, (_, taken)| free.saturating_sub(*taken));
            if needed > free {
                outcome = Err(insufficient(free, needed));
                return false;
            }
            let booked = (federation.to_string(), needed);
            submitting.insert(intent.to_string(), booked);
            true
        });
        outcome.map(|()| Reservation {
            core: Arc::clone(self),
            intent: intent.to_string(),
        })
    }

    /// An `Err` here means no row and no payment. From `Ok` on, the outcome
    /// arrives as [`Event::Operation`].
    pub(super) async fn confirm_send(self: &Arc<Self>, preview_id: &str) -> Result<OperationId> {
        let saving = self.state().saving_previews.remove(preview_id);
        if let Some(held) = saving {
            return self.confirm_saving_send(held);
        }
        let HeldSend {
            federation, quote, ..
        } = self.consume(preview_id)?;
        self.ensure_open()?;
        let client = self.connected(&federation)?;
        let quote = fresh_quote(&*client, quote).await?;
        let available = client.balance().await?;
        let intent = fresh_id("send");
        let reservation = self.reserve(&federation, &intent, quote.total()?, available)?;
        // After the reservation: `shutdown` closes the gate, then waits for those.
        self.ensure_open()?;
        let now = (self.clock)();
        let (kind, watch_kind) = match quote {
            Quote::Lightning(_) => (HistoryKind::Paid, WatchKind::LnSend),
            Quote::OnChain(_) => (HistoryKind::Sent { change: None }, WatchKind::Withdraw),
        };
        let item = HistoryItem {
            id: intent.clone(),
            kind,
            amount: quote.amount(),
            incoming: false,
            status: HistoryStatus::Pending,
            timestamp: now,
            txid: None,
            fee: Some(quote.fee()),
            session: None,
        };
        let watch = Watch {
            id: intent.clone(),
            federation_id: federation.clone(),
            kind: watch_kind,
            created_at: now,
        };
        self.store().add_tracked(&item, &watch)?;
        self.announce(&federation, &item, &HistoryWrite::Added);
        self.report(&intent, OperationState::Broadcasting, None);
        // A task of its own: a caller that goes away must not cut a payment off.
        let core = Arc::clone(self);
        tokio::spawn(async move { core.submit(&*client, &watch, &quote, reservation).await });
        Ok(intent)
    }

    /// The reservation outlives the writes here: until they are done, a resolver
    /// must not read a missing operation as a payment never sent.
    async fn submit(
        &self,
        client: &dyn FederationClient,
        watch: &Watch,
        quote: &Quote,
        reservation: Reservation,
    ) {
        #[cfg(feature = "testing")]
        if *held(&self.halt) == Some(SendHalt::BeforeSubmit) {
            return;
        }
        let submitted = match quote {
            Quote::Lightning(quote) => client.pay_invoice(quote, &watch.id).await,
            Quote::OnChain(quote) => client.withdraw(quote, &watch.id).await,
        };
        #[cfg(feature = "testing")]
        if *held(&self.halt) == Some(SendHalt::AfterSubmit) {
            return;
        }
        let saved = match submitted {
            Ok(operation) => self.follow_operation(watch, &operation),
            // Fedimint commits the operation and its transaction together, so
            // an error means nothing left the wallet.
            Err(error) => self.finish_send(
                watch,
                HistoryStatus::Failed,
                OperationState::Failed { error },
            ),
        };
        // The row stays open, and the next connection settles it.
        if let Err(e) = saved {
            eprintln!("fedimix: saving what became of send {}: {e}", watch.id);
        }
        drop(reservation);
    }

    fn follow_operation(&self, watch: &Watch, operation: &str) -> Result<()> {
        self.store().set_operation(&watch.id, operation)?;
        self.start_watching(watch.clone());
        Ok(())
    }

    /// The row first, so a crash between the writes replays and changes nothing.
    /// The event goes out on a replay too: a front end that came back from that
    /// crash lists the operation as running and waits for its end.
    fn finish_send(
        &self,
        watch: &Watch,
        status: HistoryStatus,
        state: OperationState,
    ) -> Result<()> {
        self.finish_row(watch, |row| HistoryItem { status, ..row }, state)
    }

    fn finish_row(
        &self,
        watch: &Watch,
        finished: impl FnOnce(HistoryItem) -> HistoryItem,
        state: OperationState,
    ) -> Result<()> {
        let federation = &watch.federation_id;
        let row = self
            .store()
            .history_item(WalletKind::Spending, Some(federation), &watch.id)?;
        let history_id = row.as_ref().map(|row| row.id.clone());
        if let Some(row) = row {
            self.record(federation, &finished(row))?;
        }
        self.store().finish_watch(&watch.id)?;
        self.report(&watch.id, state, history_id);
        Ok(())
    }

    pub(super) fn report(&self, id: &str, state: OperationState, history_id: Option<String>) {
        self.events.emit(&Event::Operation {
            id: id.to_string(),
            kind: OperationKind::Send,
            state,
            history_id,
        });
    }

    /// `watch` is keyed on the intent, as the row is. The amount stays the one
    /// the row was written with: what was sent, as previewed. So does the fee,
    /// unless the operation names one: details that cannot be read report zero.
    pub(super) fn apply_send(&self, watch: &Watch, update: &WatchUpdate) -> Result<()> {
        let (status, state) = match update {
            WatchUpdate::Confirming { .. } => return Ok(()),
            WatchUpdate::Settled { fee, txid, .. } => {
                let finished = |row: HistoryItem| HistoryItem {
                    status: HistoryStatus::Complete,
                    fee: paid_fee(*fee).or(row.fee),
                    txid: txid.clone().or(row.txid.clone()),
                    ..row
                };
                return self.finish_row(watch, finished, OperationState::Done);
            }
            WatchUpdate::Refunded => (HistoryStatus::Refunded, failed_with(CAME_BACK)),
            WatchUpdate::Expired => (HistoryStatus::Failed, failed_with(EXPIRED)),
            WatchUpdate::Failed { reason } => (HistoryStatus::Failed, failed_with(reason)),
        };
        self.finish_send(watch, status, state)
    }

    /// What to ask the client to follow for `watch`: for a send its operation,
    /// and `None` while it has none.
    pub(super) fn followed(&self, watch: &Watch) -> Option<Watch> {
        if !is_send(watch.kind) {
            return Some(watch.clone());
        }
        let operation = self.store().operation_of(&watch.id);
        let operation = operation
            .inspect_err(|e| eprintln!("fedimix: reading send {}: {e}", watch.id))
            .ok()??;
        Some(Watch {
            id: operation,
            ..watch.clone()
        })
    }

    /// Ends the sends with no operation, before a client database is rebuilt:
    /// a new operation log cannot say whether they went out, so "not found"
    /// will mean nothing. Written first, as a recovery cut off resumes through
    /// `open` and never comes back here.
    pub(super) fn finish_lost_intents(&self, federation: &str) -> Result<()> {
        for watch in self.unsent(federation) {
            self.finish_send(&watch, HistoryStatus::Failed, failed_with(LOST_RECORD))?;
        }
        Ok(())
    }

    /// The sends of `federation` with no operation that no call is working on.
    pub(super) fn unsent(&self, federation: &str) -> Vec<Watch> {
        let sends = match self.store().sends() {
            Ok(sends) => sends,
            Err(e) => {
                eprintln!("fedimix: reading the sends of federation {federation}: {e}");
                return Vec::new();
            }
        };
        let submitting = self.submitting.borrow();
        sends
            .into_iter()
            .filter(|(watch, operation)| {
                operation.is_none()
                    && watch.federation_id == federation
                    && !submitting.contains_key(&watch.id)
            })
            .map(|(watch, _)| watch)
            .collect()
    }

    pub(super) fn running_sends(&self) -> Result<Vec<RunningOperation>> {
        let sends = self.store().sends()?;
        let mut running: Vec<_> = sends
            .into_iter()
            .map(|(watch, _)| RunningOperation {
                id: watch.id,
                kind: OperationKind::Send,
                state: OperationState::Broadcasting,
            })
            .collect();
        running.extend(self.running_saving()?);
        Ok(running)
    }

    /// Whether every submission ended in time.
    pub(super) async fn submissions_ended(&self) -> bool {
        let mut submitting = self.submitting.subscribe();
        let ended = submitting.wait_for(Submitting::is_empty);
        tokio::time::timeout(SUBMISSION_GRACE, ended).await.is_ok()
    }
}

/// Settles an intent a crash left with no operation. Never pays: found resumes
/// the payment, and not found ends it. A failed lookup leaves the row for the
/// next time the federation comes online.
pub(super) async fn resolve_intent(core: &Core, client: &dyn FederationClient, watch: &Watch) {
    let settled = match client.find_operation(&watch.id).await {
        Ok(Some(operation)) => core.follow_operation(watch, &operation),
        Ok(None) => core.finish_send(watch, HistoryStatus::Failed, failed_with(NEVER_SENT)),
        Err(e) => Err(e),
    };
    if let Err(e) = settled {
        eprintln!("fedimix: looking up send {}: {e}", watch.id);
    }
}
