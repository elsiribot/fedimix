//! Operation lifecycle: previews, state events, cancellation, exits.

use std::sync::Arc;
use std::time::Duration;

use fedimix_core::OperationState::{
    AwaitingDevice, Broadcasting, Cancelled, Done, Failed, Signing,
};
use fedimix_core::*;

const FAST: DemoTiming = DemoTiming {
    step: Duration::from_millis(5),
    preview_ttl: Duration::from_secs(600),
    broadcast_hold: Duration::ZERO,
    send_lock_timeout: None,
};
const EXPIRED: DemoTiming = DemoTiming {
    preview_ttl: Duration::ZERO,
    ..FAST
};
const ADDRESS: &str = "bc1qdemo0destination";

fn demo(scenario: DemoScenario, timing: DemoTiming) -> (Fedimix, EventStream) {
    let backend: Arc<DemoBackend> = DemoBackend::with_timing(scenario, timing);
    let core = Fedimix::new(backend);
    let events = core.subscribe();
    (core, events)
}

fn fast(scenario: DemoScenario) -> (Fedimix, EventStream) {
    demo(scenario, FAST)
}

fn send(wallet: WalletKind, sat: u64) -> SendRequest {
    SendRequest {
        wallet,
        destination: ADDRESS.into(),
        amount: SendAmount::Exact(Amount::from_sat(sat)),
        fee_rate_sat_vb: None,
    }
}

async fn next_state(events: &EventStream, id: &str) -> (OperationKind, OperationState) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("timed out")
            .expect("stream ended");
        if let Event::Operation {
            id: got,
            kind,
            state,
            ..
        } = event
            && got == id
        {
            return (kind, state);
        }
    }
}

/// Every state of operation `id` up to and including the final one.
async fn states(events: &EventStream, id: &str) -> (OperationKind, Vec<OperationState>) {
    let mut seen = Vec::new();
    loop {
        let (kind, state) = next_state(events, id).await;
        let last = state.is_final();
        seen.push(state);
        if last {
            return (kind, seen);
        }
    }
}

#[tokio::test]
async fn saving_send_walks_through_the_device_states() {
    let (core, events) = fast(DemoScenario::Funded);
    let before = core.saving_state().await.unwrap();
    let preview = core
        .prepare_send(send(WalletKind::Saving, 1_000_000))
        .await
        .unwrap();
    let id = core.confirm_send(preview.id).await.unwrap();

    // confirm_send returned at once: nothing has moved yet.
    assert_eq!(core.saving_state().await.unwrap().balance, before.balance);

    let (kind, seen) = states(&events, &id).await;
    assert_eq!(kind, OperationKind::Send);
    assert_eq!(seen, vec![AwaitingDevice, Signing, Broadcasting, Done]);
    let after = core.saving_state().await.unwrap();
    assert!(after.balance < before.balance);
    assert_eq!(after.history.len(), before.history.len() + 1);
}

#[tokio::test]
async fn spending_send_needs_no_device() {
    let (core, events) = fast(DemoScenario::Funded);
    let preview = core
        .prepare_send(send(WalletKind::Spending, 1_000_000))
        .await
        .unwrap();
    let id = core.confirm_send(preview.id).await.unwrap();
    assert_eq!(states(&events, &id).await.1, vec![Broadcasting, Done]);
}

#[tokio::test]
async fn refill_walks_through_the_device_states() {
    let (core, events) = fast(DemoScenario::BelowTarget);
    let preview = core.prepare_refill().await.unwrap();
    let id = core.confirm_refill(preview.id).await.unwrap();
    let (kind, seen) = states(&events, &id).await;
    assert_eq!(kind, OperationKind::Refill);
    assert_eq!(seen, vec![AwaitingDevice, Signing, Broadcasting, Done]);
}

#[tokio::test]
async fn refill_overshoot_joins_the_queue() {
    let (core, events) = fast(DemoScenario::BelowTarget);
    let before = core.spending_state().await.unwrap();
    let preview = core.prepare_refill().await.unwrap();
    assert_eq!(
        preview.overshoot.msat,
        // Coins of 0.06, fee 420 sat, gap of 0.0388.
        Amount::from_sat(6_000_000 - 420 - 3_880_000).msat
    );
    let id = core.confirm_refill(preview.id.clone()).await.unwrap();
    states(&events, &id).await;

    let after = core.spending_state().await.unwrap();
    let queue = core.queue_state().await.unwrap();
    let denoms: Vec<_> = queue
        .items
        .iter()
        .map(|i| i.denomination.as_sat())
        .collect();
    assert_eq!(
        denoms,
        vec![2_000_000, 100_000],
        "1-2-5 split, largest first"
    );
    assert_eq!(after.moving, queue.total);
    // What is too small for a denomination stays in Spending.
    assert_eq!(after.for_spending, Amount::from_sat(5_000_000 + 19_580));
    assert_eq!(
        after.for_spending.checked_add(after.moving),
        Some(after.balance)
    );
    assert_eq!(
        after.balance,
        before
            .balance
            .checked_add(preview.amount)
            .and_then(|b| b.checked_sub(preview.fee))
            .unwrap()
    );
}

#[tokio::test]
async fn refill_selection_covers_the_gap_after_the_fee() {
    let (core, _events) = fast(DemoScenario::BelowTarget);
    // A gap of exactly 0.01: the 0.01 coin alone falls short by the fee.
    let settings = Settings {
        spending_target: Amount::from_sat(2_120_000),
        ..core.settings().await.unwrap()
    };
    core.set_settings(settings).await.unwrap();
    let preview = core.prepare_refill().await.unwrap();
    assert_eq!(preview.coins.len(), 2);
}

#[tokio::test]
async fn exit_preview_carries_the_real_fee() {
    let (core, events) = fast(DemoScenario::Funded);
    let before = core.spending_state().await.unwrap();
    let item = core.queue_state().await.unwrap().items[0].clone();
    let preview = core.prepare_exit(item.id.clone()).await.unwrap();
    assert_eq!(preview.item_id, item.id);
    assert_eq!(preview.fee, Amount::from_sat(420));

    let id = core.exit_now(preview.id.clone()).await.unwrap();
    let (kind, state) = next_state(&events, &id).await;
    assert_eq!((kind, state), (OperationKind::Exit, Broadcasting));

    // While it runs the item shows as withdrawing and cannot exit again.
    let during = core.queue_state().await.unwrap();
    assert!(matches!(
        during.items[0].status,
        QueueItemStatus::Withdrawing { .. }
    ));
    assert!(matches!(
        core.prepare_exit(item.id.clone()).await,
        Err(FedimixError::UnknownItem(_))
    ));

    assert_eq!(states(&events, &id).await.1, vec![Done]);
    let queue = core.queue_state().await.unwrap();
    assert!(queue.items.iter().all(|i| i.id != item.id));
    let after = core.spending_state().await.unwrap();
    assert_eq!(after.moving, queue.total);
    let paid = item.denomination.checked_add(preview.fee).unwrap();
    assert_eq!(after.balance, before.balance.checked_sub(paid).unwrap());
    assert_eq!(
        after.for_spending.checked_add(after.moving),
        Some(after.balance)
    );
    let exited = core.saving_state().await.unwrap();
    assert!(exited.coins.iter().any(|c| c.amount == item.denomination
        && matches!(c.tag, PrivacyTag::EarlyExit { .. })));
}

#[tokio::test]
async fn exit_preview_refusals() {
    let (core, _) = fast(DemoScenario::Funded);
    assert!(matches!(
        core.prepare_exit("nope".into()).await,
        Err(FedimixError::UnknownItem(_))
    ));
    let (core, _) = fast(DemoScenario::Offline);
    assert_eq!(
        core.prepare_exit("queue-1".into()).await,
        Err(FedimixError::Offline)
    );
    let (core, _) = fast(DemoScenario::NoSaving);
    assert!(matches!(
        core.prepare_exit("queue-1".into()).await,
        Err(FedimixError::NotSetUp(_))
    ));
    // Spending holds 200 sat, the fee is 420.
    let (core, _) = fast(DemoScenario::ExitDeferred);
    assert!(matches!(
        core.prepare_exit("queue-1".into()).await,
        Err(FedimixError::InsufficientFunds { .. })
    ));
}

#[tokio::test]
async fn prepare_exit_refuses_grace_period() {
    // queue-3 is deferred for the online grace period; no manual override.
    let (core, _) = fast(DemoScenario::ExitDeferred);
    assert_eq!(
        core.prepare_exit("queue-3".into()).await,
        Err(FedimixError::GracePeriod)
    );
}

#[tokio::test]
async fn cancel_stops_a_send_before_it_broadcasts() {
    let (core, events) = fast(DemoScenario::Funded);
    let before = core.saving_state().await.unwrap();
    let preview = core
        .prepare_send(send(WalletKind::Saving, 1_000_000))
        .await
        .unwrap();
    let id = core.confirm_send(preview.id.clone()).await.unwrap();
    core.cancel_operation(id.clone()).await.unwrap();

    let (_, seen) = states(&events, &id).await;
    assert_eq!(seen.last(), Some(&Cancelled));
    assert!(!seen.contains(&Broadcasting));
    assert_eq!(core.saving_state().await.unwrap(), before);
    assert_eq!(
        core.confirm_send(preview.id).await,
        Err(FedimixError::Cancelled)
    );
}

#[tokio::test]
async fn cancel_is_too_late_once_broadcasting() {
    let (core, events) = fast(DemoScenario::Funded);
    let preview = core
        .prepare_send(send(WalletKind::Saving, 1_000_000))
        .await
        .unwrap();
    let id = core.confirm_send(preview.id).await.unwrap();
    while next_state(&events, &id).await.1 != Broadcasting {}
    core.cancel_operation(id.clone()).await.unwrap();
    assert_eq!(states(&events, &id).await.1, vec![Done]);
}

#[tokio::test]
async fn cancel_of_an_unknown_operation_errors() {
    let (core, _) = fast(DemoScenario::Funded);
    assert!(matches!(
        core.cancel_operation("nope".into()).await,
        Err(FedimixError::UnknownItem(_))
    ));
}

#[tokio::test]
async fn previews_expire() {
    let (core, _) = demo(DemoScenario::Funded, EXPIRED);
    let preview = core
        .prepare_send(send(WalletKind::Saving, 1_000_000))
        .await
        .unwrap();
    assert_eq!(
        core.confirm_send(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
    let item = core.queue_state().await.unwrap().items[0].id.clone();
    let preview = core.prepare_exit(item).await.unwrap();
    assert_eq!(
        core.exit_now(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );

    let (core, _) = demo(DemoScenario::BelowTarget, EXPIRED);
    let preview = core.prepare_refill().await.unwrap();
    assert_eq!(
        core.confirm_refill(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
}

#[tokio::test]
async fn previews_state_when_they_expire() {
    let (core, _) = fast(DemoScenario::Funded);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let send = core
        .prepare_send(send(WalletKind::Saving, 1_000_000))
        .await
        .unwrap();
    assert!(send.expires_at >= now + 590);
    let exit = core.prepare_exit("queue-1".into()).await.unwrap();
    assert!(exit.expires_at >= now + 590);
    let (core, _) = fast(DemoScenario::BelowTarget);
    assert!(core.prepare_refill().await.unwrap().expires_at >= now + 590);
}

#[tokio::test]
async fn discarded_previews_are_gone_and_discarding_is_idempotent() {
    let (core, _) = fast(DemoScenario::Funded);
    let preview = core
        .prepare_send(send(WalletKind::Saving, 1_000_000))
        .await
        .unwrap();
    core.discard_preview(preview.id.clone()).await.unwrap();
    core.discard_preview(preview.id.clone()).await.unwrap();
    assert!(matches!(
        core.confirm_send(preview.id).await,
        Err(FedimixError::UnknownItem(_))
    ));
    core.discard_preview("never-existed".into()).await.unwrap();
}

#[tokio::test]
async fn a_preview_confirms_once() {
    let (core, events) = fast(DemoScenario::Funded);
    let preview = core
        .prepare_send(send(WalletKind::Saving, 1_000_000))
        .await
        .unwrap();
    let id = core.confirm_send(preview.id.clone()).await.unwrap();
    assert!(matches!(
        core.confirm_send(preview.id).await,
        Err(FedimixError::UnknownItem(_))
    ));
    states(&events, &id).await;
}

#[tokio::test]
async fn device_rejection_fails_the_operation_and_moves_nothing() {
    let (core, events) = fast(DemoScenario::DeviceRejects);
    let before = core.saving_state().await.unwrap();
    let preview = core
        .prepare_send(send(WalletKind::Saving, 1_000_000))
        .await
        .unwrap();
    let id = core.confirm_send(preview.id).await.unwrap();
    let (_, seen) = states(&events, &id).await;
    assert_eq!(
        seen,
        vec![
            AwaitingDevice,
            Signing,
            Failed {
                error: FedimixError::DeviceRejected
            }
        ]
    );
    assert_eq!(core.saving_state().await.unwrap(), before);
}

#[tokio::test]
async fn device_rejection_fails_setup_too() {
    let (core, _) = fast(DemoScenario::DeviceRejects);
    let device = core.detect_devices().await.unwrap().devices.remove(0);
    assert_eq!(
        core.setup_saving(device.id).await,
        Err(FedimixError::DeviceRejected)
    );
}
