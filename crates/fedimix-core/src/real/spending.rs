//! Snapshots, built from what the backend already holds. Nothing here touches
//! the network.

use crate::api::*;

/// `(for_spending, moving)`: the balance up to the target stays, the rest
/// heads for Saving.
fn split(balance: Amount, target: Amount) -> (Amount, Amount) {
    (balance.min(target), balance.saturating_sub(target))
}

pub(super) fn spending_state(
    federation: Option<FederationInfo>,
    total_balance: Amount,
    target: Amount,
    saving_ready: bool,
    notes: Vec<NoteBucket>,
    history: Vec<HistoryItem>,
) -> SpendingState {
    let balance = federation.as_ref().map_or(Amount::ZERO, |f| f.balance);
    let (for_spending, moving) = split(balance, target);
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

/// Nothing queues until Saving exists; `moving` waits in Spending.
pub(super) fn queue_state() -> QueueState {
    QueueState {
        total: Amount::ZERO,
        anonymity_estimate: 0,
        typical_exit_secs: 0,
        waiting_for_saving: true,
        items: Vec::new(),
    }
}
