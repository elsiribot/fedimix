use crate::api::{Amount, HistoryItem, OperationId, OperationKind, OperationState, WalletKind};

#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    DepositConfirmed { amount: Amount },
    ExitLanded { amount: Amount },
    SendConfirmed { amount: Amount },
    SavingSetupNudge { amount: Amount },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    BalanceChanged {
        wallet: WalletKind,
        balance: Amount,
    },
    HistoryItemAdded {
        wallet: WalletKind,
        item: HistoryItem,
    },
    HistoryItemUpdated {
        wallet: WalletKind,
        item: HistoryItem,
    },
    QueueChanged,
    ConnectionChanged {
        online: bool,
    },
    FederationChanged,
    SavingChanged,
    SettingsChanged,
    Notification(Notice),
    Operation {
        id: OperationId,
        kind: OperationKind,
        state: OperationState,
        /// The history row the operation made, once it exists (as a rule
        /// with `Done`). Look it up with `history`.
        history_id: Option<String>,
    },
}

/// Stream of backend events. Ends (`None`) once all senders are dropped.
///
/// Clones share one queue: each event reaches exactly one clone, so a clone
/// is another consumer of the same work, not a second copy of the stream.
/// Call `subscribe()` for an independent stream.
#[derive(Debug, Clone)]
pub struct EventStream {
    rx: async_channel::Receiver<Event>,
}

impl EventStream {
    pub fn new(rx: async_channel::Receiver<Event>) -> Self {
        Self { rx }
    }

    pub async fn next(&self) -> Option<Event> {
        self.rx.recv().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn event_stream_ends_when_sender_dropped() {
        let (tx, rx) = async_channel::unbounded();
        let stream = EventStream::new(rx);
        tx.send(Event::QueueChanged).await.unwrap();
        drop(tx);
        assert_eq!(stream.next().await, Some(Event::QueueChanged));
        assert_eq!(stream.next().await, None);
    }
}
