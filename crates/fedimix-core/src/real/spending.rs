//! Snapshots, built from what the backend already holds. Nothing here touches
//! the network.

use crate::api::*;

/// `moving` is what the queue holds; the rest of the balance is for spending.
pub(super) fn spending_state(
    federation: Option<FederationInfo>,
    total_balance: Amount,
    target: Amount,
    moving: Amount,
    saving_ready: bool,
    notes: Vec<NoteBucket>,
    history: Vec<HistoryItem>,
) -> SpendingState {
    let balance = federation.as_ref().map_or(Amount::ZERO, |f| f.balance);
    let for_spending = balance.saturating_sub(moving);
    SpendingState {
        federation,
        balance,
        total_balance,
        target,
        for_spending,
        moving,
        saving_ready,
        notes,
        history,
        features: Features {
            saving: true,
            ..Features::NONE
        },
    }
}
