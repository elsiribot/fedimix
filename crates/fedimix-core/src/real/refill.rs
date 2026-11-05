//! Refilling Spending from Saving: the smallest coins that cover the gap to
//! the target go, whole, to a deposit address of the active federation.
//!
//! A refill is a Saving payment like a send (`saving_send.rs`): the same one
//! operation at a time, device, stored transaction, rebroadcast and stalled
//! handling, and the same rule for the federation's address. It spends every
//! coin of a stalled payment first, so it conflicts with each. What differs is
//! the selection: no coin selection of BDK's, no change output, and nothing
//! recorded as change, as the whole output is the refill.

use std::collections::HashSet;
use std::sync::Arc;

use bitcoin::OutPoint;

use super::Core;
use super::saving_send::{FROZEN_STALLED, HeldSaving, NO_ACCOUNT};
use super::send::{PREVIEW_TTL, fresh_id};
use crate::api::*;
use crate::error::{FedimixError, Result};
use crate::saving::{Labels, RefillPlan};

const AT_TARGET: &str = "Spending is at or above its target, so there is nothing to refill.";
const NO_FEDERATION: &str = "Join a federation first: a refill pays into it.";

impl Core {
    /// How far the active federation's balance is below the target. Refused
    /// at or above it.
    fn refill_gap(&self) -> Result<Amount> {
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
        let gap = target.saturating_sub(Amount { msat: balance });
        if gap == Amount::ZERO {
            return Err(FedimixError::InvalidAmount(AT_TARGET.into()));
        }
        Ok(gap)
    }

    pub(super) async fn prepare_refill(&self) -> Result<RefillPreview> {
        self.ensure_running()?;
        let wallet = self.scanned_wallet()?;
        self.refuse_if_busy()?;
        let account = self.store().device_account()?;
        let account = account.ok_or_else(|| FedimixError::NotSetUp(NO_ACCOUNT.into()))?;
        self.refuse_jade_behind_proxy(Some(&account))?;
        let gap = self.refill_gap()?;
        let fee_rate = self.saving_fee_rate(None).await?;
        let must_spend = self.stalled_inputs(&wallet)?;
        let spends: HashSet<OutPoint> = must_spend.iter().copied().collect();
        // What is delivered must exceed the margin of change, twice the
        // minimum deposit, as the minimum moves on v2.
        let (federation, margin) = self
            .change_federation(&spends)
            .await?
            .ok_or_else(|| FedimixError::NotSetUp(NO_FEDERATION.into()))?;
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
            gap,
            floor: margin,
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
            overshoot: delivered_amount.saturating_sub(gap),
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
            delivered: Some(delivered),
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
