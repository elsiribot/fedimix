//! Sending from Saving, with change into the federation.
//!
//! A preview holds an unsigned PSBT. Confirming takes the one Saving operation:
//! `AwaitingDevice`, `Signing`, `Broadcasting`, `Done`. Until the device has
//! signed, only memory changes, so a rejection, a cancel or a crash leaves
//! nothing. The signed transaction is then stored before anything is sent, and
//! from that row on the send belongs to the store, not to this process or to the
//! wallet's view of the chain:
//!
//! - `Signed` is an operation still running, also across a restart: it is
//!   broadcast until a server has it, and no other Saving operation can start.
//! - `Seen` went out. A lagging server can forget it, which frees its coins in
//!   BDK's view, so they stay out of selection here, and it is rebroadcast. It
//!   stays open until 6 blocks deep, as a reorg can undo less.
//! - `Stalled` went out, or may have, and no server takes it now. A miner can
//!   still have it, so nothing frees its coins: no count, no timer, no refusal.
//!   It stays on show, is rebroadcast, and the next send must spend its coins,
//!   so that at most one of the two can confirm.
//! - A rebroadcast cannot pay twice: it is the same transaction.
//!
//! Only two things end a send for good: a confirmed conflict 6 blocks deep, and
//! a node's judgement that it is invalid on a first attempt, when nothing of
//! ours has left. A policy refusal there stalls it. A reply that judges nothing
//! is retried, for at most `NO_VERDICT_LIMIT` from the stored row, then stalls.
//! Of sends sharing a coin only the newest is offered or holds the coins; a
//! send another confirmed against keeps its row, saying which. The first broadcast makes a chain client per
//! attempt from the settings in force, so closing the sync's client for an
//! abandoned call cannot touch it. A rebroadcast uses the sync's client, and a
//! change of settings ends it with the sync (`supervise_saving`).

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::psbt::Psbt;
use bitcoin::{Address, FeeRate, OutPoint, Transaction};
use tokio::sync::Notify;

#[cfg(feature = "testing")]
use super::held;
use super::saving::{JADE_PROXY, stopping};
#[cfg(feature = "testing")]
use super::send::SendHalt;
use super::send::{PREVIEW_TTL, Reservation, fresh_id};
use super::{Core, Gate};
use crate::api::*;
use crate::destination::{DestinationKind, parse_destination, requested_amount, validate_address};
use crate::error::{FedimixError, Result};
use crate::event::Event;
use crate::saving::{
    ChainClient, DeviceAccount, Labels, SavingWallet, SendPlan, Signer, TxPosition, is_invalid,
    is_refusal, sort_history,
};
use crate::store::{SignedState, SignedTx};

/// The confirmation target of the fee estimate.
const FEE_TARGET_BLOCKS: u16 = 6;
const REGTEST_FEE_RATE: u32 = 2;
/// Change goes to the federation only above this many minimum deposits, as the
/// minimum moves on v2.
const CHANGE_MARGIN: u64 = 2;
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(60);
/// How deep a block must lie before no reorg is to be expected: a transaction
/// or a conflict there settles a stored transaction.
const SETTLED_DEPTH: u32 = 6;
/// How long a first broadcast may go without a verdict before it stalls,
/// counted from the stored row, so a restart does not start it over.
const NO_VERDICT_LIMIT: Timestamp = 30 * 60;

const SAME_ADDRESS: &str =
    "Your last change is still reaching Spending. Try again in a few minutes.";
const ONE_AT_A_TIME: &str =
    "Another Saving payment is still running. Wait for it to end, then try again.";
const ON_CHAIN_ONLY: &str = "Saving sends on chain, not over Lightning.";
const WHOLE_SATS: &str = "An on-chain amount must be a whole number of sats.";
const ZERO_FEE_RATE: &str = "The fee rate must be at least 1 sat/vB.";
const HUGE_FEE_RATE: &str = "This fee rate is too high.";
const OWN_ADDRESS: &str =
    "This is your federation's deposit address. To move Saving into Spending, use Refill.";
const REPLACES: &str = "An earlier payment has not confirmed and may still go through. This send spends the same coins, so only one of the two can confirm.";
const STALLED_COIN_GONE: &str = "A coin of a payment that has not confirmed is missing from the chain server's answer, so the next send cannot be made to conflict with it. Try again after the next sync.";
const FROZEN_STALLED: &str = "A coin you froze is tied to a payment that has not confirmed. Unfreeze it to send: the next send must spend it, so that only one of the two can confirm.";
const NO_ESTIMATE: &str =
    "The chain server has no fee estimate right now. Enter a fee rate, or try again later.";
const NO_FEDERATION: &str = "No federation is active, so the change stays in Saving.";
const BAD_DEPOSIT_ADDRESS: &str = "The federation gave a deposit address Fedimix cannot use.";
const TAMPERED: &str = "Your device handed back a different transaction from the one you reviewed, so Fedimix did not send it.";
const NO_ACCOUNT: &str = "Set up Saving with a hardware wallet first.";
const UNREADABLE_TX: &str = "A stored Saving transaction cannot be read.";

/// The federation a preview pays its change to.
#[derive(Debug)]
struct ChangeFederation {
    id: String,
    address: String,
    /// Follows the deposit, on the generation that has watches.
    watch: Option<String>,
}

/// A Saving preview that can still be confirmed.
pub(super) struct HeldSaving {
    psbt: Psbt,
    fee: Amount,
    change: ChangeDestination,
    change_vout: Option<u32>,
    /// `Some` only when the PSBT pays it.
    federation: Option<ChangeFederation>,
    generation: u64,
    expires_at: Timestamp,
}

/// The running Saving operation.
pub(super) struct SavingOp {
    id: String,
    state: OperationState,
    cancel: Arc<Notify>,
    cancelled: bool,
}

/// Lets go of the operation at the end of its task, also through a panic.
struct OpGuard {
    core: Arc<Core>,
    id: String,
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        let mut live = self.core.saving();
        if live.op.as_ref().is_some_and(|op| op.id == self.id) {
            live.op = None;
        }
    }
}

fn internal(sentence: &str) -> FedimixError {
    FedimixError::Internal(sentence.to_string())
}

/// Refused for an amount with a part of a sat in it.
fn to_btc(amount: Amount) -> Result<bitcoin::Amount> {
    if !amount.msat.is_multiple_of(1000) {
        return Err(FedimixError::InvalidAmount(WHOLE_SATS.into()));
    }
    Ok(bitcoin::Amount::from_sat(amount.as_sat()))
}

fn from_btc(amount: bitcoin::Amount) -> Amount {
    Amount::from_sat(amount.to_sat())
}

/// The rate of an estimate: never below 1 sat/vB, which is all servers relay.
fn rate_of(estimate: Option<f64>, network: Network) -> Result<FeeRate> {
    match estimate {
        Some(sat_vb) => {
            // sat/vB is 250 sat/kwu. Up, as a little more is no harm.
            let kwu = (sat_vb.max(1.0) * 250.0).ceil() as u64;
            Ok(FeeRate::from_sat_per_kwu(kwu))
        }
        None if network == Network::Regtest => Ok(FeeRate::from_sat_per_vb_u32(REGTEST_FEE_RATE)),
        None => Err(FedimixError::NotSetUp(NO_ESTIMATE.into())),
    }
}

fn tx_of(row: &SignedTx) -> Result<Transaction> {
    deserialize_hex(&row.tx_hex).map_err(|_| internal(UNREADABLE_TX))
}

/// What a device handed back pays what was reviewed, down to the fee. The fee
/// is read again because a PSBT carries its input amounts apart from the
/// transaction.
fn same_as_reviewed(signed: &Psbt, reviewed: &Psbt, fee: Amount) -> bool {
    signed.unsigned_tx == reviewed.unsigned_tx
        && signed.fee().is_ok_and(|paid| from_btc(paid) == fee)
}

impl Core {
    fn chain_client(&self) -> Result<Arc<dyn ChainClient>> {
        let target = self.chain.borrow().clone();
        target.connect(&self.chain_factory)
    }

    async fn saving_fee_rate(&self, requested: Option<u64>) -> Result<FeeRate> {
        if let Some(rate) = requested {
            if rate == 0 {
                return Err(FedimixError::InvalidAmount(ZERO_FEE_RATE.into()));
            }
            return FeeRate::from_sat_per_vb(rate)
                .ok_or_else(|| FedimixError::InvalidAmount(HUGE_FEE_RATE.into()));
        }
        let estimate = self
            .chain_client()?
            .fee_rate_sat_vb(FEE_TARGET_BLOCKS)
            .await?;
        rate_of(estimate, self.config.network)
    }

    /// Where change can go, with the margin: `None` with no active federation.
    /// Asks for the minimum first, so a failure uses up no address.
    async fn change_federation(
        &self,
        spends: &HashSet<OutPoint>,
    ) -> Result<Option<(ChangeFederation, Amount)>> {
        let Some(id) = self.store().active_federation()? else {
            return Ok(None);
        };
        self.ensure_open()?;
        let client = self.connected(&id)?;
        let min_deposit = client.min_deposit().await?;
        let (address, watch) = client.deposit_address().await?;
        if self.address_held(&address, spends)? {
            return Err(FedimixError::NotSetUp(SAME_ADDRESS.into()));
        }
        let margin = Amount {
            msat: min_deposit.msat.saturating_mul(CHANGE_MARGIN),
        };
        Ok(Some((ChangeFederation { id, address, watch }, margin)))
    }

    fn checked_script(&self, address: &str) -> Result<bitcoin::ScriptBuf> {
        let parsed = Address::from_str(address).map_err(|_| internal(BAD_DEPOSIT_ADDRESS))?;
        let network = match self.config.network {
            Network::Bitcoin => bitcoin::Network::Bitcoin,
            Network::Signet => bitcoin::Network::Signet,
            Network::Regtest => bitcoin::Network::Regtest,
        };
        let checked = parsed
            .require_network(network)
            .map_err(|_| internal(BAD_DEPOSIT_ADDRESS))?;
        Ok(checked.script_pubkey())
    }

    /// The open transactions, oldest first, each with whether a newer one
    /// that is not dropped spends one of its coins. Of a set that shares coins
    /// only the newest is ever offered to a server and holds the coins: the
    /// older ones are what the user replaced, and the newest's coins cover theirs.
    fn open_ledger(&self) -> Result<Vec<(SignedTx, Transaction, bool)>> {
        let rows = self.store().open_signed()?;
        let txs = rows.iter().map(tx_of).collect::<Result<Vec<_>>>()?;
        let spent: Vec<HashSet<OutPoint>> = txs
            .iter()
            .map(|tx| tx.input.iter().map(|input| input.previous_output).collect())
            .collect();
        let superseded = (0..rows.len())
            .map(|i| ((i + 1)..rows.len()).any(|j| !spent[i].is_disjoint(&spent[j])))
            .collect::<Vec<_>>();
        Ok(rows
            .into_iter()
            .zip(txs)
            .zip(superseded)
            .map(|((row, tx), superseded)| (row, tx, superseded))
            .collect())
    }

    /// The coins signed away by a transaction not yet settled, whatever the
    /// wallet's view says about them now.
    fn locked_outpoints(&self) -> Result<HashSet<OutPoint>> {
        let mut locked = HashSet::new();
        for (row, tx, superseded) in self.open_ledger()? {
            if !superseded && row.state != SignedState::Stalled {
                locked.extend(tx.input.iter().map(|input| input.previous_output));
            }
        }
        Ok(locked)
    }

    /// What the next send must spend, so that it conflicts with every stalled
    /// send that is the newest of its set. A coin the wallet does not show is
    /// fine when a stored send of ours spends it in the wallet's view: the two
    /// already conflict. Otherwise no conflict can be made, which is refused.
    fn stalled_inputs(&self, wallet: &SavingWallet) -> Result<Vec<OutPoint>> {
        let ledger = self.open_ledger()?;
        let locked = self.locked_outpoints()?;
        let mut inputs = Vec::new();
        for (row, tx, superseded) in &ledger {
            if row.state == SignedState::Stalled && !superseded {
                inputs.extend(tx.input.iter().map(|input| input.previous_output));
            }
        }
        inputs.retain(|outpoint| !locked.contains(outpoint));
        inputs.sort();
        inputs.dedup();
        let mut usable = Vec::new();
        for outpoint in inputs {
            if wallet.has_coin(&outpoint.to_string()) {
                usable.push(outpoint);
                continue;
            }
            let spent_by_ours = ledger.iter().any(|(_, tx, _)| {
                wallet.tx_position(tx.compute_txid()) != TxPosition::Absent
                    && tx
                        .input
                        .iter()
                        .any(|input| input.previous_output == outpoint)
            });
            if !spent_by_ours {
                return Err(FedimixError::NotSetUp(STALLED_COIN_GONE.into()));
            }
        }
        Ok(usable)
    }

    /// Whether a send spending `spends` must keep off `address`: it is, while
    /// any stored transaction that may still pay it is one this send does not
    /// conflict with. Only one of two that conflict can pay it.
    fn address_held(&self, address: &str, spends: &HashSet<OutPoint>) -> Result<bool> {
        for row in self.store().signed_paying(address)? {
            let tx = tx_of(&row)?;
            let conflicts = tx
                .input
                .iter()
                .any(|input| spends.contains(&input.previous_output));
            let final_or_replaced = row.state == SignedState::Stalled && conflicts;
            if !final_or_replaced {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The sentence for a send that lost to `winner`.
    fn lost_reason(
        &self,
        wallet: &SavingWallet,
        winner: Option<(&SignedTx, &Transaction)>,
        labels: &Labels,
    ) -> String {
        let Some((row, tx)) = winner else {
            return "Not sent: another transaction that spends the same coins confirmed instead."
                .to_string();
        };
        let item = wallet.signed_item(tx, HistoryStatus::Complete, row.created_at, labels);
        let to = wallet.recipient(tx, row.change_address.as_deref());
        let to = to.unwrap_or_else(|| "an address of yours".to_string());
        format!(
            "Not sent: your earlier payment of {} to {to} confirmed instead, as both spent the same coins.",
            item.amount
        )
    }

    /// The row of a stored send the wallet's view does not list, if it has one.
    fn signed_row(
        &self,
        wallet: &SavingWallet,
        labels: &Labels,
        row: &SignedTx,
        tx: &Transaction,
        superseded: bool,
    ) -> Result<Option<HistoryItem>> {
        if wallet.tx_position(tx.compute_txid()) != TxPosition::Absent {
            return Ok(None);
        }
        if let Some(lost_to) = &row.lost_to {
            let winner = self.store().signed_after(lost_to, &row.id)?;
            // A later send of ours won: the user replaced this one.
            if winner.as_ref().is_some_and(|(_, later)| *later) {
                return Ok(None);
            }
            let winner = winner
                .map(|(winner, _)| tx_of(&winner).map(|tx| (winner, tx)))
                .transpose()?;
            let reason = self.lost_reason(wallet, winner.as_ref().map(|(r, t)| (r, t)), labels);
            let status = HistoryStatus::Lost { reason };
            return Ok(Some(wallet.signed_item(tx, status, row.created_at, labels)));
        }
        if row.state == SignedState::Stalled && !superseded {
            let status = HistoryStatus::Stalled;
            return Ok(Some(wallet.signed_item(tx, status, row.created_at, labels)));
        }
        Ok(None)
    }

    /// Adds to a list from the wallet the stored sends it no longer shows: the
    /// newest stalled one of each set, and any that another confirmed instead.
    /// No row of a send the user saw `Done` vanishes.
    pub(super) fn add_signed_rows(
        &self,
        wallet: &SavingWallet,
        labels: &Labels,
        items: &mut Vec<HistoryItem>,
    ) -> Result<()> {
        let rows = self.store().signed_for_history()?;
        let ledger = self.open_ledger()?;
        for row in &rows {
            let tx = tx_of(row)?;
            let superseded = ledger
                .iter()
                .any(|(open, _, superseded)| open.id == row.id && *superseded);
            if let Some(item) = self.signed_row(wallet, labels, row, &tx, superseded)? {
                items.push(item);
            }
        }
        sort_history(items);
        Ok(())
    }

    /// A Jade's PIN step cannot use the proxy: better said at the review.
    fn refuse_jade_behind_proxy(&self, account: Option<&DeviceAccount>) -> Result<()> {
        let jade = account.is_some_and(|account| account.kind == DeviceKind::Jade);
        if jade && self.chain.borrow().proxy.is_some() {
            return Err(FedimixError::NotSetUp(JADE_PROXY.into()));
        }
        Ok(())
    }

    fn refuse_if_busy(&self) -> Result<()> {
        let signed = self.store().open_signed()?;
        let unsent = signed.iter().any(|row| row.state == SignedState::Signed);
        if unsent || self.saving().op.is_some() {
            return Err(FedimixError::NotSetUp(ONE_AT_A_TIME.into()));
        }
        Ok(())
    }

    pub(super) async fn prepare_saving_send(&self, request: SendRequest) -> Result<SendPreview> {
        let destination = parse_destination(&request.destination)?;
        if destination.kind != DestinationKind::OnChain {
            return Err(FedimixError::InvalidDestination(ON_CHAIN_ONLY.into()));
        }
        validate_address(&destination.normalized, self.config.network)?;
        let amount = match requested_amount(request.amount, destination.amount)? {
            SendAmount::Exact(amount) => Some(to_btc(amount)?),
            _ => None,
        };
        self.ensure_running()?;
        let wallet = self.scanned_wallet()?;
        self.refuse_if_busy()?;
        let account = self.store().device_account()?;
        self.refuse_jade_behind_proxy(account.as_ref())?;
        let fee_rate = self.saving_fee_rate(request.fee_rate_sat_vb).await?;
        let must_spend = self.stalled_inputs(&wallet)?;
        let spends: HashSet<OutPoint> = must_spend.iter().copied().collect();
        // A send of everything makes no change, so it needs no federation.
        let federation = if amount.is_some() {
            self.change_federation(&spends).await?
        } else {
            None
        };
        // Paying it would hide the rest of the coin, and Refill does this job.
        let paying_federation = federation
            .as_ref()
            .is_some_and(|(federation, _)| federation.address == destination.normalized);
        let recorded = !self
            .store()
            .signed_paying(&destination.normalized)?
            .is_empty();
        if paying_federation || (amount.is_some() && recorded) {
            return Err(FedimixError::InvalidDestination(OWN_ADDRESS.into()));
        }
        let to = self.checked_script(&destination.normalized)?;
        let change_to = federation
            .as_ref()
            .map(|(federation, _)| self.checked_script(&federation.address))
            .transpose()?;

        let labels = Labels::load(&self.store())?;
        let locked = self.locked_outpoints()?;
        if must_spend
            .iter()
            .any(|outpoint| labels.is_frozen(&outpoint.to_string()))
        {
            return Err(FedimixError::NotSetUp(FROZEN_STALLED.into()));
        }
        let mut notes = Vec::new();
        if !must_spend.is_empty() {
            notes.push(REPLACES.to_string());
        }
        let mut plan = SendPlan {
            to,
            amount,
            fee_rate,
            change_to,
            locked,
            must_spend,
            only_must_spend: false,
        };
        let first = wallet.build_send(&labels, &plan)?;
        let (built, federation, change) = match (amount, federation, first.change) {
            (Some(_), Some((federation, margin)), Some((_, value))) if from_btc(value) > margin => {
                let change = ChangeDestination::Spending {
                    amount: from_btc(value),
                };
                (first, Some(federation), change)
            }
            (Some(_), federation, Some(_)) => {
                // The same coins: others could leave large change here.
                plan.change_to = None;
                plan.must_spend = first
                    .psbt
                    .unsigned_tx
                    .input
                    .iter()
                    .map(|input| input.previous_output)
                    .collect();
                plan.only_must_spend = true;
                let built = wallet.build_send(&labels, &plan)?;
                if federation.is_none() {
                    notes.push(NO_FEDERATION.to_string());
                }
                let change = built.change.map_or(ChangeDestination::None, |(_, value)| {
                    ChangeDestination::SmallChange {
                        amount: from_btc(value),
                    }
                });
                (built, None, change)
            }
            (_, _, _) => (first, None, ChangeDestination::None),
        };

        let coins = wallet.coins(&labels);
        let inputs = built
            .psbt
            .unsigned_tx
            .input
            .iter()
            .map(|input| {
                let outpoint = input.previous_output.to_string();
                let coin = coins.iter().find(|coin| coin.outpoint == outpoint);
                SendInput {
                    amount: coin.map_or(Amount::ZERO, |coin| coin.amount),
                    tag: coin.map(|coin| coin.tag.clone()),
                    outpoint_or_note: outpoint,
                }
            })
            .collect();
        let usable =
            wallet
                .usable_coins(&labels, &plan.locked)
                .iter()
                .fold(Amount::ZERO, |total, coin| Amount {
                    msat: total.msat.saturating_add(coin.amount.msat),
                });
        let held_back = wallet.balance().saturating_sub(usable);
        if held_back > Amount::ZERO {
            notes.push(format!(
                "{held_back} of your balance cannot be sent yet: it is frozen, still unconfirmed, or tied to a send that has not settled."
            ));
        }
        let paid = built
            .psbt
            .unsigned_tx
            .output
            .iter()
            .find(|output| output.script_pubkey == plan.to);
        let preview = SendPreview {
            id: fresh_id("preview"),
            wallet: WalletKind::Saving,
            destination: destination.normalized,
            amount: paid.map_or(Amount::ZERO, |output| from_btc(output.value)),
            fee: from_btc(built.fee),
            federation: federation.as_ref().map(|f| f.id.clone()),
            inputs,
            change,
            notes,
            expires_at: (self.clock)() + PREVIEW_TTL,
        };
        let held = HeldSaving {
            psbt: built.psbt,
            fee: preview.fee,
            change: preview.change.clone(),
            change_vout: built.change.map(|(vout, _)| vout),
            federation,
            generation: self.saving().generation,
            expires_at: preview.expires_at,
        };
        self.state()
            .saving_previews
            .insert(preview.id.clone(), held);
        self.review_opened.notify_one();
        Ok(preview)
    }

    /// Whether a Send review is open now.
    pub(super) fn review_open(&self) -> bool {
        let now = (self.clock)();
        let state = self.state();
        state.saving_previews.values().any(|p| p.expires_at > now)
    }

    /// What must still hold of a preview, before the device is asked to sign.
    fn check_preview(&self, held: &HeldSaving, wallet: &SavingWallet) -> Result<()> {
        if held.expires_at <= (self.clock)() {
            return Err(FedimixError::PreviewExpired);
        }
        self.check_active(held)?;
        let spends: HashSet<OutPoint> = held
            .psbt
            .unsigned_tx
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect();
        if let Some(federation) = &held.federation {
            self.ensure_open()?;
            self.connected(&federation.id)?;
            if self.address_held(&federation.address, &spends)? {
                return Err(FedimixError::NotSetUp(SAME_ADDRESS.into()));
            }
        }
        let labels = Labels::load(&self.store())?;
        let usable = wallet.usable_coins(&labels, &self.locked_outpoints()?);
        let still_usable = held.psbt.unsigned_tx.input.iter().all(|input| {
            let outpoint = input.previous_output.to_string();
            usable.iter().any(|coin| coin.outpoint == outpoint)
        });
        // A send that stalled since the preview must be conflicted too.
        let conflicts = self
            .stalled_inputs(wallet)?
            .iter()
            .all(|outpoint| spends.contains(outpoint));
        if !still_usable || !conflicts {
            return Err(FedimixError::PreviewExpired);
        }
        Ok(())
    }

    /// Change must reach the federation that is active now.
    fn check_active(&self, held: &HeldSaving) -> Result<()> {
        if let Some(federation) = &held.federation {
            let active = self.store().active_federation()?;
            if active.as_deref() != Some(&federation.id) {
                return Err(FedimixError::PreviewExpired);
            }
        }
        Ok(())
    }

    /// Takes the one operation, if this preview is not older than the last.
    fn claim_operation(&self, id: &str, generation: u64) -> Result<Arc<Notify>> {
        let cancel = Arc::new(Notify::new());
        let mut live = self.saving();
        if live.op.is_some() {
            return Err(FedimixError::NotSetUp(ONE_AT_A_TIME.into()));
        }
        if live.generation != generation {
            return Err(FedimixError::PreviewExpired);
        }
        live.generation += 1;
        live.last_op = Some(id.to_string());
        live.op = Some(SavingOp {
            id: id.to_string(),
            state: OperationState::AwaitingDevice,
            cancel: Arc::clone(&cancel),
            cancelled: false,
        });
        Ok(cancel)
    }

    /// An `Err` here means nothing happened. From `Ok` on, the outcome arrives
    /// as [`Event::Operation`].
    pub(super) fn confirm_saving_send(self: &Arc<Self>, held: HeldSaving) -> Result<OperationId> {
        self.refuse_if_busy()?;
        self.ensure_running()?;
        let wallet = self.saving_wallet()?;
        self.check_preview(&held, &wallet)?;
        let id = fresh_id("saving");
        // Books nothing: it keeps `shutdown` waiting for the task's writes.
        let reservation = self.reserve("", &id, Amount::ZERO, Amount::ZERO)?;
        let cancel = self.claim_operation(&id, held.generation)?;
        let guard = OpGuard {
            core: Arc::clone(self),
            id: id.clone(),
        };
        // After the reservation: `shutdown` closes the gate, then waits for those.
        self.ensure_running()?;
        self.report(&id, OperationState::AwaitingDevice, None);
        // A task of its own: a caller that goes away must not cut a send off.
        let core = Arc::clone(self);
        let task_id = id.clone();
        tokio::spawn(async move {
            core.run_send(task_id, held, wallet, cancel, guard, reservation)
                .await;
        });
        Ok(id)
    }

    fn set_op_state(&self, id: &str, state: OperationState) {
        if let Some(op) = self.saving().op.as_mut().filter(|op| op.id == id) {
            op.state = state.clone();
        }
        self.report(id, state, None);
    }

    /// Ends the wait at the device. `false` for an id that is no Saving send.
    pub(super) fn cancel_saving(&self, id: &str) -> Result<bool> {
        {
            let mut live = self.saving();
            if let Some(op) = live.op.as_mut().filter(|op| op.id == id) {
                if op.state != OperationState::Broadcasting {
                    op.cancelled = true;
                    op.cancel.notify_one();
                }
                return Ok(true);
            }
            if live.last_op.as_deref() == Some(id) {
                return Ok(true);
            }
        }
        self.store().is_signed(id)
    }

    /// From here on the send cannot be cancelled; `false` if it was just now.
    fn enter_broadcasting(&self, id: &str) -> bool {
        let mut live = self.saving();
        let Some(op) = live.op.as_mut().filter(|op| op.id == id) else {
            return false;
        };
        if op.cancelled {
            return false;
        }
        op.state = OperationState::Broadcasting;
        true
    }

    pub(super) fn running_saving(&self) -> Result<Vec<RunningOperation>> {
        let mut running: Vec<_> = self
            .saving()
            .op
            .iter()
            .map(|op| RunningOperation {
                id: op.id.clone(),
                kind: OperationKind::Send,
                state: op.state.clone(),
            })
            .collect();
        for row in self.store().open_signed()? {
            if row.state == SignedState::Signed && running.iter().all(|op| op.id != row.id) {
                running.push(RunningOperation {
                    id: row.id,
                    kind: OperationKind::Send,
                    state: OperationState::Broadcasting,
                });
            }
        }
        Ok(running)
    }

    /// The device of `account`, when it can be opened. Waits for it to appear:
    /// "Connect your device" is a step of the review, until the preview expires.
    async fn wait_for_device(
        &self,
        account: &DeviceAccount,
        expires_at: Timestamp,
    ) -> Result<Arc<dyn Signer>> {
        self.refuse_jade_behind_proxy(Some(account))?;
        loop {
            if expires_at <= (self.clock)() {
                return Err(FedimixError::PreviewExpired);
            }
            let found = self.detect_devices().await?;
            if let Some(device) = found.iter().find(|device| device.kind == account.kind) {
                match self.open_device(&device.id, Some(account)).await {
                    Ok((_, signer)) => return Ok(signer),
                    Err(FedimixError::DeviceNotConnected(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            tokio::time::sleep(FIRST_RETRY).await;
        }
    }

    async fn sign(&self, id: &str, held: &HeldSaving) -> Result<Psbt> {
        let account = self.store().device_account()?;
        let account = account.ok_or_else(|| FedimixError::NotSetUp(NO_ACCOUNT.into()))?;
        let signer = self.wait_for_device(&account, held.expires_at).await?;
        self.set_op_state(id, OperationState::Signing);
        let mut psbt = held.psbt.clone();
        signer.sign(&mut psbt).await?;
        Ok(psbt)
    }

    /// Everything a restart needs, the signed row last: that row is the commit
    /// point, and what comes before it is harmless without it.
    async fn store_signed(
        &self,
        id: &str,
        tx: &Transaction,
        held: &HeldSaving,
        wallet: &Arc<SavingWallet>,
    ) -> Result<SignedTx> {
        // A change address that goes out must never be handed out again.
        let saving = Arc::clone(wallet);
        tokio::task::spawn_blocking(move || saving.save())
            .await
            .map_err(|_| internal(UNREADABLE_TX))??;
        // A rebroadcast of what this replaces may have landed while the device
        // signed: then that one holds the coins again, and this must not go.
        let locked = self.locked_outpoints()?;
        if tx
            .input
            .iter()
            .any(|input| locked.contains(&input.previous_output))
        {
            return Err(FedimixError::PreviewExpired);
        }
        let txid = tx.compute_txid().to_string();
        let change = match held.change {
            ChangeDestination::None => None,
            ChangeDestination::Spending { amount } => Some(SentChange {
                amount,
                to_spending: true,
            }),
            ChangeDestination::SmallChange { amount } => Some(SentChange {
                amount,
                to_spending: false,
            }),
        };
        let small = change.as_ref().is_some_and(|change| !change.to_spending);
        {
            let store = self.store();
            let mut labels = Labels::load(&store)?;
            if let (true, Some(vout)) = (small, held.change_vout) {
                let outpoint = format!("{txid}:{vout}");
                labels.set_tag(&store, &outpoint, crate::saving::Tag::SmallChange, None)?;
            }
            labels.record_sent(&store, &txid, change)?;
        }
        let federation = held.federation.as_ref();
        if let Some(ChangeFederation {
            id: federation,
            watch: Some(watch),
            ..
        }) = federation
        {
            self.add_deposit_watch(federation, watch.clone())?;
        }
        let row = SignedTx {
            id: id.to_string(),
            txid,
            tx_hex: serialize_hex(tx),
            state: SignedState::Signed,
            change_address: federation.map(|federation| federation.address.clone()),
            created_at: (self.clock)(),
            lost_to: None,
        };
        self.store().add_signed(&row)?;
        Ok(row)
    }

    async fn run_send(
        self: Arc<Self>,
        id: String,
        held: HeldSaving,
        wallet: Arc<SavingWallet>,
        cancel: Arc<Notify>,
        guard: OpGuard,
        reservation: Reservation,
    ) {
        let _held_until_the_end = (guard, reservation);
        let mut gate = self.gate.subscribe();
        let signed = tokio::select! {
            signed = self.sign(&id, &held) => signed,
            () = cancel.notified() => Err(FedimixError::Cancelled),
            () = stopping(&mut gate) => Err(FedimixError::Cancelled),
        };
        let stored = match signed {
            Ok(psbt) if !same_as_reviewed(&psbt, &held.psbt, held.fee) => Err(internal(TAMPERED)),
            Ok(_) if self.check_active(&held).is_err() => Err(FedimixError::PreviewExpired),
            Ok(psbt) => match wallet.finalize(psbt) {
                Ok(tx) if tx.compute_txid() != held.psbt.unsigned_tx.compute_txid() => {
                    Err(internal(TAMPERED))
                }
                Ok(_) if !self.enter_broadcasting(&id) => Err(FedimixError::Cancelled),
                Ok(tx) => self
                    .store_signed(&id, &tx, &held, &wallet)
                    .await
                    .map(|row| (row, tx)),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        let (row, tx) = match stored {
            Ok(stored) => stored,
            Err(FedimixError::Cancelled) => {
                self.finish_op(&id, OperationState::Cancelled, None);
                return;
            }
            Err(error) => {
                self.finish_op(&id, OperationState::Failed { error }, None);
                return;
            }
        };
        self.report(&id, OperationState::Broadcasting, None);
        self.settle(&wallet, &row, &tx, false).await;
    }

    /// Clears the operation before telling anyone, so that whoever hears of its
    /// end finds Saving free.
    fn finish_op(&self, id: &str, state: OperationState, history_id: Option<String>) {
        {
            let mut live = self.saving();
            if live.op.as_ref().is_some_and(|op| op.id == id) {
                live.op = None;
            }
        }
        self.report(id, state, history_id);
    }

    /// One attempt to have a server hold `tx`. When an earlier attempt may have
    /// got through, it asks first, so nothing is sent twice for no reason.
    async fn reach_server(
        &self,
        wallet: &SavingWallet,
        tx: &Transaction,
        doubt: bool,
    ) -> Result<()> {
        let txid = tx.compute_txid();
        if wallet.tx_position(txid) != TxPosition::Absent {
            return Ok(());
        }
        let chain = self.chain_client()?;
        if doubt && chain.tx_seen(txid).await? {
            return Ok(());
        }
        chain.broadcast(tx).await
    }

    /// Broadcasts until a server has `tx` or says no. A stop leaves the row, and
    /// the next start comes back here. `doubt`: an attempt may already have got
    /// through.
    async fn settle(
        self: &Arc<Self>,
        wallet: &Arc<SavingWallet>,
        row: &SignedTx,
        tx: &Transaction,
        mut doubt: bool,
    ) {
        #[cfg(feature = "testing")]
        if *held(&self.halt) == Some(SendHalt::BeforeSubmit) {
            return;
        }
        let mut gate = self.gate.subscribe();
        let mut target = self.chain.subscribe();
        let mut delay = FIRST_RETRY;
        loop {
            let attempt = tokio::select! {
                attempt = self.reach_server(wallet, tx, doubt) => attempt,
                () = stopping(&mut gate) => return,
                _ = target.changed() => {
                    doubt = true;
                    continue;
                }
            };
            match attempt {
                Ok(()) => break,
                // Nothing of ours left: a node judged our only attempt invalid.
                Err(error) if is_invalid(&error) && !doubt => {
                    self.end_signed(row, SignedState::Dropped, OperationState::Failed { error })
                        .await;
                    return;
                }
                // One node's policy: a node that relays more may take it.
                Err(error) if is_refusal(&error) && !doubt => {
                    self.end_signed(row, SignedState::Stalled, OperationState::Failed { error })
                        .await;
                    return;
                }
                // It may be out there, so a refusal proves nothing.
                Err(error) if is_refusal(&error) => {
                    let error = FedimixError::SendInDoubt;
                    self.end_signed(row, SignedState::Stalled, OperationState::Failed { error })
                        .await;
                    return;
                }
                Err(error) => {
                    doubt = true;
                    if error != FedimixError::Offline {
                        eprintln!("fedimix: broadcasting {}: {error}", row.id);
                    }
                }
            }
            // No verdict for this long: hold it as stalled, which frees nothing
            // and gives the way out a running send does not.
            if (self.clock)() >= row.created_at.saturating_add(NO_VERDICT_LIMIT) {
                let error = FedimixError::SendInDoubt;
                self.end_signed(row, SignedState::Stalled, OperationState::Failed { error })
                    .await;
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(delay) => delay = (delay * 2).min(MAX_RETRY),
                () = stopping(&mut gate) => return,
                _ = target.changed() => {}
            }
        }
        #[cfg(feature = "testing")]
        if *held(&self.halt) == Some(SendHalt::AfterSubmit) {
            return;
        }
        self.reached_server(wallet, row, tx).await;
    }

    /// Writes `state`, trying until it holds: what the user is told next must
    /// be what a restart finds. `false` when Fedimix stopped first, and the row
    /// then says the send still runs, which the next start takes up.
    async fn write_state(&self, id: &str, state: SignedState) -> bool {
        let mut gate = self.gate.subscribe();
        loop {
            let Err(e) = self.store().set_signed_state(id, state) else {
                return true;
            };
            eprintln!("fedimix: saving the state of {id}: {e}");
            tokio::select! {
                () = tokio::time::sleep(FIRST_RETRY) => {}
                () = stopping(&mut gate) => return false,
            }
        }
    }

    /// The server has it, whatever fails from here: no failure is reported.
    async fn reached_server(&self, wallet: &Arc<SavingWallet>, row: &SignedTx, tx: &Transaction) {
        let txid = tx.compute_txid();
        if wallet.tx_position(txid) == TxPosition::Absent {
            insert_sent(wallet, tx).await;
        }
        if self.write_state(&row.id, SignedState::Seen).await {
            self.finish_op(&row.id, OperationState::Done, Some(txid.to_string()));
            self.events.emit(&Event::SavingChanged);
            self.saving_wake.notify_one();
        }
    }

    /// Ends the operation, the row first.
    async fn end_signed(&self, row: &SignedTx, state: SignedState, outcome: OperationState) {
        if self.write_state(&row.id, state).await {
            self.finish_op(&row.id, outcome, None);
            self.events.emit(&Event::SavingChanged);
        }
    }

    /// Looks after what went out, once a sync has brought the wallet up to
    /// date. Whether anything changed. The store decides everything here, so a
    /// restart loses nothing. `Signed` rows belong to `settle`.
    pub(super) async fn tend_signed(
        &self,
        wallet: &Arc<SavingWallet>,
        chain: &dyn ChainClient,
    ) -> bool {
        let tended = self.tend_rows(wallet, chain).await;
        tended.unwrap_or_else(|e| {
            eprintln!("fedimix: looking after stored Saving transactions: {e}");
            false
        })
    }

    async fn tend_rows(&self, wallet: &Arc<SavingWallet>, chain: &dyn ChainClient) -> Result<bool> {
        let ledger = self.open_ledger()?;
        let mut changed = false;
        for (row, tx, superseded) in &ledger {
            if row.state == SignedState::Signed {
                continue;
            }
            let next = match wallet.tx_position(tx.compute_txid()) {
                TxPosition::Confirmed { depth } if depth >= SETTLED_DEPTH => {
                    Some(SignedState::Confirmed)
                }
                TxPosition::Confirmed { .. } | TxPosition::Pending => {
                    if row.lost_to.is_some() {
                        self.store().set_lost_to(&row.id, None)?;
                        changed = true;
                    }
                    Some(SignedState::Seen)
                }
                TxPosition::Absent => {
                    let lost = self.note_lost(wallet, row, tx, &ledger)?;
                    changed |= lost.is_some();
                    match wallet.conflict(tx) {
                        Some((_, depth)) if depth >= SETTLED_DEPTH => Some(SignedState::Dropped),
                        // A confirmed conflict: offering it is pointless.
                        Some(_) => None,
                        None => self.tend_absent(wallet, chain, row, tx, *superseded).await,
                    }
                }
            };
            if let Some(next) = next.filter(|next| *next != row.state) {
                self.store().set_signed_state(&row.id, next)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Records what confirmed instead of `row`, once, and tells the history: a
    /// send reported `Done` must not change in silence.
    fn note_lost(
        &self,
        wallet: &SavingWallet,
        row: &SignedTx,
        tx: &Transaction,
        ledger: &[(SignedTx, Transaction, bool)],
    ) -> Result<Option<String>> {
        let Some((winner, _)) = wallet.conflict(tx) else {
            return Ok(None);
        };
        let winner = winner.to_string();
        if row.lost_to.as_ref() == Some(&winner) {
            return Ok(None);
        }
        self.store().set_lost_to(&row.id, Some(&winner))?;
        let row = SignedTx {
            lost_to: Some(winner.clone()),
            ..row.clone()
        };
        let superseded = ledger
            .iter()
            .any(|(open, _, superseded)| open.id == row.id && *superseded);
        let labels = Labels::load(&self.store())?;
        if let Some(item) = self.signed_row(wallet, &labels, &row, tx, superseded)? {
            self.events.emit(&Event::HistoryItemUpdated {
                wallet: WalletKind::Saving,
                item,
            });
        }
        self.events.emit(&Event::SavingChanged);
        Ok(Some(winner))
    }

    /// A send the wallet's view does not show and nothing confirmed against.
    /// Only the newest of a set that shares coins is offered, so a send the user
    /// replaced is never revived, also once its replacement stalls.
    async fn tend_absent(
        &self,
        wallet: &Arc<SavingWallet>,
        chain: &dyn ChainClient,
        row: &SignedTx,
        tx: &Transaction,
        superseded: bool,
    ) -> Option<SignedState> {
        // A send now running must spend a stalled send's coins: offering the
        // stalled one would race it.
        if superseded || (row.state == SignedState::Stalled && self.saving().op.is_some()) {
            return None;
        }
        match chain.broadcast(tx).await {
            Ok(()) => {
                insert_sent(wallet, tx).await;
                Some(SignedState::Seen)
            }
            // Nobody answered, so this says nothing yet.
            Err(FedimixError::Offline) => None,
            Err(error) => {
                eprintln!("fedimix: rebroadcasting {}: {error}", row.id);
                Some(SignedState::Stalled)
            }
        }
    }
}

/// A failed write stays owed: the wallet holds it and the next sync writes it.
async fn insert_sent(wallet: &Arc<SavingWallet>, tx: &Transaction) {
    let (wallet, tx) = (Arc::clone(wallet), tx.clone());
    let inserted = tokio::task::spawn_blocking(move || wallet.insert_sent(&tx)).await;
    if !matches!(inserted, Ok(Ok(()))) {
        eprintln!("fedimix: a send went out, and saving it in the wallet failed");
    }
}

/// Takes up a send that a stop left with no server known to have it. An attempt
/// may have got through before the stop, so it is in doubt from the start.
pub(super) async fn resume_signed(
    core: Arc<Core>,
    wallet: Arc<SavingWallet>,
    row: SignedTx,
) -> Result<()> {
    if *core.gate.borrow() == Gate::Stopped {
        return Ok(());
    }
    match tx_of(&row) {
        Ok(tx) => core.settle(&wallet, &row, &tx, true).await,
        Err(e) => eprintln!("fedimix: resuming {}: {e}", row.id),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_comes_from_the_estimate_or_a_sentence() {
        let vb = |rate: FeeRate| rate.to_sat_per_kwu() as f64 / 250.0;
        assert_eq!(vb(rate_of(Some(3.5), Network::Bitcoin).unwrap()), 3.5);
        assert_eq!(vb(rate_of(Some(0.2), Network::Bitcoin).unwrap()), 1.0);
        assert_eq!(vb(rate_of(None, Network::Regtest).unwrap()), 2.0);
        for network in [Network::Bitcoin, Network::Signet] {
            assert_eq!(
                rate_of(None, network),
                Err(FedimixError::NotSetUp(NO_ESTIMATE.into()))
            );
        }
    }

    #[test]
    fn amounts_are_whole_sats() {
        assert_eq!(
            to_btc(Amount::from_sat(5)),
            Ok(bitcoin::Amount::from_sat(5))
        );
        assert!(to_btc(Amount { msat: 5_500 }).is_err());
    }
}
