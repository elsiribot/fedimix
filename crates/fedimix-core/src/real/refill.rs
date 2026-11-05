//! Refilling Spending from Saving: the smallest coins that cover the gap to
//! the target go, whole, to a deposit address of the active federation.
//!
//! A refill is a Saving payment like a send (`saving_send.rs`): the same one
//! operation at a time, device, stored transaction, rebroadcast and stalled
//! handling, and the same rule for the federation's address. It spends every
//! coin of a stalled payment first, so it conflicts with each. What differs is
//! the selection: no coin selection of BDK's, no change output, and nothing
//! recorded as change, as the whole output is the refill.
//!
//! Spending's target is a cap on what sits with the federation, so a refill
//! counts what is already on its way there, and the deposit fee, and never
//! runs beside another refill. Nor does it replace a stalled send: that would
//! cancel a payment to someone else behind the user's back.

use std::collections::HashSet;
use std::sync::Arc;

use bitcoin::OutPoint;

use super::Core;
use super::saving_send::{FROZEN_STALLED, HeldSaving, NO_ACCOUNT, SETTLED_DEPTH, margin_of};
use super::send::{PREVIEW_TTL, fresh_id};
use crate::api::*;
use crate::error::{FedimixError, Result};
use crate::saving::{Labels, RefillPlan, SavingWallet};
use crate::store::SignedState;

const AT_TARGET: &str = "Spending is at or above its target, so there is nothing to refill.";
const NO_FEDERATION: &str = "Join a federation first: a refill pays into it.";
const ON_ITS_WAY: &str = "Spending reaches its target with the money already on its way to it, so there is nothing to refill.";

impl Core {
    /// The active federation, its balance and the target. Refused at or above
    /// the target.
    fn refill_target(&self) -> Result<(String, Amount, Amount)> {
        let (rows, active, target) = {
            let store = self.store();
            (
                store.federations()?,
                store.active_federation()?,
                store.settings()?.spending_target,
            )
        };
        let active = active.ok_or_else(|| FedimixError::NotSetUp(NO_FEDERATION.into()))?;
        let balance = rows
            .iter()
            .find(|row| row.id == active)
            .map_or(0, |row| row.balance_msat);
        let balance = Amount { msat: balance };
        if balance >= target {
            return Err(FedimixError::InvalidAmount(AT_TARGET.into()));
        }
        Ok((active, balance, target))
    }

    /// What is on its way to `federation` and not in its balance yet, less the
    /// deposit fee each pays: its deposits not settled, and what Saving signed
    /// to its addresses that it has not seen. Each counts once: a deposit row
    /// that names a stored transaction stands for it. A v1 deposit confirming
    /// shows what arrived on chain, before the fee; a v2 one pending, after it.
    /// A stalled payment is not counted, as it may never arrive.
    fn on_its_way(&self, federation: &str, deposit_fee: Amount) -> Result<Amount> {
        let deposits = self.store().deposit_rows(federation)?;
        let mut total = Amount::ZERO;
        let mut seen = HashSet::new();
        for row in &deposits {
            seen.extend(row.txid.clone());
            let arrives = match row.status {
                HistoryStatus::Pending => row.amount,
                HistoryStatus::Confirming { .. } => row.amount.saturating_sub(deposit_fee),
                _ => continue,
            };
            total = total.checked_add(arrives).unwrap_or(total);
        }
        for (row, tx, superseded) in self.open_ledger()? {
            // A row from before schema 9 names no federation, and counts here.
            let elsewhere = row
                .federation_id
                .as_deref()
                .is_some_and(|id| id != federation);
            if superseded
                || elsewhere
                || row.state == SignedState::Stalled
                || seen.contains(&row.txid)
            {
                continue;
            }
            let Some(script) = row
                .change_address
                .as_deref()
                .and_then(|address| self.checked_script(address).ok())
            else {
                continue;
            };
            for output in tx.output.iter().filter(|o| o.script_pubkey == script) {
                let arrives = Amount::from_sat(output.value.to_sat()).saturating_sub(deposit_fee);
                total = total.checked_add(arrives).unwrap_or(total);
            }
        }
        Ok(total)
    }

    /// What a refill must deliver now: the gap left after what is on its way,
    /// plus the deposit fee, so that Spending lands at or above the target.
    fn refill_needs(&self, federation: &str, deposit_fee: Amount) -> Result<Amount> {
        let (_, balance, target) = self.refill_target()?;
        let coming = self.on_its_way(federation, deposit_fee)?;
        let gap = target.saturating_sub(balance).saturating_sub(coming);
        if gap == Amount::ZERO {
            return Err(FedimixError::InvalidAmount(ON_ITS_WAY.into()));
        }
        Ok(gap.checked_add(deposit_fee).unwrap_or(gap))
    }

    /// What no refill may run beside: another refill not final, which is money
    /// on its way, or a stalled send that is the newest of its coins, which a
    /// refill would cancel. Each sentence says when the block lifts.
    fn refuse_refill_now(&self, wallet: &SavingWallet) -> Result<()> {
        let ledger = self.open_ledger()?;
        let refill = ledger
            .iter()
            .rev()
            .find(|(row, _, _)| row.kind == OperationKind::Refill);
        if let Some((row, _, superseded)) = refill {
            let sentence = if *superseded {
                format!(
                    "A send from Saving replaced your last refill. Refill again once that send is {SETTLED_DEPTH} blocks deep."
                )
            } else if row.state == SignedState::Stalled {
                format!(
                    "No chain server holds your last refill now, and it may still go through. Refill again once it is {SETTLED_DEPTH} blocks deep, or once a send from Saving that replaces it is."
                )
            } else {
                format!(
                    "Your last refill is not yet {SETTLED_DEPTH} blocks deep. Refill again once it is."
                )
            };
            return Err(FedimixError::NotSetUp(sentence));
        }
        // One a newer send replaced is covered by that send's coins: the user
        // chose to replace it, and only one of the two can confirm.
        let stalled = ledger.iter().find(|(row, _, superseded)| {
            row.kind == OperationKind::Send && row.state == SignedState::Stalled && !superseded
        });
        if let Some((row, tx, _)) = stalled {
            let labels = Labels::load(&self.store())?;
            let item = wallet.signed_item(tx, HistoryStatus::Stalled, row.created_at, &labels);
            let to = wallet.recipient(tx, row.change_address.as_deref());
            let to = to.unwrap_or_else(|| "an address of yours".to_string());
            return Err(FedimixError::NotSetUp(format!(
                "Your payment of {} to {to} has not confirmed and may still go through. A refill would cancel it, so refill once a chain server holds it again, or after you send from Saving, which replaces it.",
                item.amount
            )));
        }
        Ok(())
    }

    /// What must still hold of a refill at confirm, beside what holds of every
    /// Saving preview: the same blocks, and the same amount to deliver, so the
    /// review's overshoot is the one that lands.
    pub(super) fn check_refill(&self, wallet: &SavingWallet, held: &HeldSaving) -> Result<()> {
        self.refuse_refill_now(wallet)?;
        let federation = held.federation.as_ref().map(|f| f.id.as_str());
        let needs = self.refill_needs(federation.unwrap_or(""), held.deposit_fee)?;
        if needs != held.needs {
            return Err(FedimixError::PreviewExpired);
        }
        Ok(())
    }

    pub(super) async fn prepare_refill(&self) -> Result<RefillPreview> {
        self.ensure_running()?;
        let wallet = self.scanned_wallet()?;
        self.refuse_if_busy()?;
        let account = self.store().device_account()?;
        let account = account.ok_or_else(|| FedimixError::NotSetUp(NO_ACCOUNT.into()))?;
        self.refuse_jade_behind_proxy(Some(&account))?;
        self.refuse_refill_now(&wallet)?;
        let (active, _, _) = self.refill_target()?;
        // Refused before a deposit address is used up, also when what is on
        // its way already fills Spending.
        self.ensure_open()?;
        let early_fee = self.connected(&active)?.min_deposit().await?;
        self.refill_needs(&active, early_fee)?;
        let fee_rate = self.saving_fee_rate(None).await?;
        let must_spend = self.stalled_inputs(&wallet)?;
        let spends: HashSet<OutPoint> = must_spend.iter().copied().collect();
        // The minimum deposit is also the deposit fee.
        let (federation, deposit_fee) = self
            .change_federation(&spends)
            .await?
            .ok_or_else(|| FedimixError::NotSetUp(NO_FEDERATION.into()))?;
        let needs = self.refill_needs(&federation.id, deposit_fee)?;
        let to = self.checked_script(&federation.address)?;
        let labels = Labels::load(&self.store())?;
        if must_spend
            .iter()
            .any(|outpoint| labels.is_frozen(&outpoint.to_string()))
        {
            return Err(FedimixError::NotSetUp(FROZEN_STALLED.into()));
        }
        let plan = RefillPlan {
            to,
            fee_rate,
            locked: self.locked_outpoints()?,
            must_spend,
            gap: needs,
            // As for change, as the minimum moves on v2.
            floor: margin_of(deposit_fee),
        };
        let (built, taken) = wallet.build_refill(&labels, &plan)?;
        let delivered = built.psbt.unsigned_tx.output[0].value;
        let fee = Amount::from_sat(built.fee.to_sat());
        let coins = wallet.coins(&labels);
        let coins: Vec<Coin> = taken
            .iter()
            .filter_map(|outpoint| {
                let outpoint = outpoint.to_string();
                coins.iter().find(|coin| coin.outpoint == outpoint).cloned()
            })
            .collect();
        let delivered_amount = Amount::from_sat(delivered.to_sat());
        let preview = RefillPreview {
            id: fresh_id("preview"),
            amount: delivered_amount
                .checked_add(fee)
                .unwrap_or(delivered_amount),
            fee,
            coins,
            // What lands above the target, the deposit fee paid.
            overshoot: delivered_amount.saturating_sub(needs),
            device_name: account.name,
            expires_at: (self.clock)() + PREVIEW_TTL,
        };
        let held = HeldSaving {
            kind: OperationKind::Refill,
            psbt: built.psbt,
            fee,
            change: ChangeDestination::None,
            change_vout: None,
            federation: Some(federation),
            deposit_fee,
            needs,
            generation: self.saving().generation,
            expires_at: preview.expires_at,
        };
        self.state()
            .saving_previews
            .insert(preview.id.clone(), held);
        self.review_opened.notify_one();
        Ok(preview)
    }

    /// As a Saving send's confirm: an `Err` means nothing happened.
    pub(super) fn confirm_refill(self: &Arc<Self>, preview_id: &str) -> Result<OperationId> {
        let held = self
            .take_saving_preview(preview_id, OperationKind::Refill)
            .ok_or_else(|| FedimixError::UnknownItem(preview_id.to_string()))?;
        self.confirm_saving(held)
    }
}
