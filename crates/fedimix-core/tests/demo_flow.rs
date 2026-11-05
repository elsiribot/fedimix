//! Walks the demo backend through the same call sequence the UI uses.

use fedimix_core::*;

const FAST: DemoTiming = DemoTiming {
    step: std::time::Duration::from_millis(1),
    preview_ttl: std::time::Duration::from_secs(600),
    broadcast_hold: std::time::Duration::ZERO,
    send_lock_timeout: None,
};

fn core(scenario: DemoScenario) -> Fedimix {
    Fedimix::new(DemoBackend::with_timing(scenario, FAST))
}

/// Wait until operation `id` reaches a final state, which must be `Done`.
async fn done(events: &EventStream, id: OperationId) {
    loop {
        match events.next().await {
            Some(Event::Operation { id: got, state, .. }) if got == id && state.is_final() => {
                assert_eq!(state, OperationState::Done);
                return;
            }
            Some(_) => {}
            None => panic!("stream ended"),
        }
    }
}

#[tokio::test]
async fn first_run_join_then_setup_saving() {
    let core = core(DemoScenario::FirstRun);
    assert!(core.spending_state().await.unwrap().federation.is_none());

    let suggested = core.suggested_federations().await.unwrap();
    let joined = core
        .join_federation(suggested[0].invite.clone())
        .await
        .unwrap();
    assert_eq!(joined.name, suggested[0].name);
    assert_eq!(core.federations().await.unwrap(), vec![joined.clone()]);
    let spending = core.spending_state().await.unwrap();
    assert_eq!(spending.federation, Some(joined));
    assert!(!spending.saving_ready);

    let device = core.detect_devices().await.unwrap().devices.remove(0);
    core.setup_saving(device.id.clone()).await.unwrap();
    assert_eq!(core.saving_state().await.unwrap().device, Some(device));
    assert!(core.spending_state().await.unwrap().saving_ready);
}

#[tokio::test]
async fn funded_send_and_exit_update_state_and_emit() {
    let core = core(DemoScenario::Funded);
    let events = core.subscribe();

    // Saving send: review, confirm, history grows and balance drops.
    let before = core.saving_state().await.unwrap();
    let preview = core
        .prepare_send(SendRequest {
            wallet: WalletKind::Saving,
            destination: "bc1qdemo0destination".into(),
            amount: SendAmount::Exact(Amount::from_sat(1_000_000)),
            fee_rate_sat_vb: None,
        })
        .await
        .unwrap();
    let op = core.confirm_send(preview.id).await.unwrap();
    done(&events, op).await;
    let after = core.saving_state().await.unwrap();
    assert_eq!(after.history.len(), before.history.len() + 1);
    assert!(after.balance < before.balance);
    assert!(matches!(after.history[0].kind, HistoryKind::Sent { .. }));

    // Exit now: the queue shrinks and a coin appears in Saving.
    let queue = core.queue_state().await.unwrap();
    let first = queue.items[0].clone();
    let exit = core.prepare_exit(first.id.clone()).await.unwrap();
    let op = core.exit_now(exit.id).await.unwrap();
    done(&events, op).await;
    let queue_after = core.queue_state().await.unwrap();
    assert_eq!(queue_after.items.len(), queue.items.len() - 1);
    assert!(queue_after.items.iter().all(|i| i.id != first.id));
    assert_eq!(
        core.saving_state().await.unwrap().coins.len(),
        after.coins.len() + 1,
        "exited note becomes a coin"
    );
    assert!(core.prepare_exit(first.id).await.is_err());
}

#[tokio::test]
async fn balance_and_queue_events_reach_a_subscriber() {
    let core = core(DemoScenario::Funded);
    let progress = core.subscribe();
    let events = core.subscribe();
    let exit = core.prepare_exit("queue-1".into()).await.unwrap();
    let op = core.exit_now(exit.id).await.unwrap();
    done(&progress, op).await;
    let mut saw_balance = false;
    let mut saw_queue = false;
    while let Ok(Some(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), events.next()).await
    {
        saw_balance |= matches!(event, Event::BalanceChanged { .. });
        saw_queue |= event == Event::QueueChanged;
    }
    assert!(saw_balance && saw_queue);
}

#[tokio::test]
async fn below_target_refill_raises_spending() {
    let core = core(DemoScenario::BelowTarget);
    let events = core.subscribe();
    let before = core.spending_state().await.unwrap();
    assert!(before.for_spending < before.target);

    let preview = core.prepare_refill().await.unwrap();
    assert!(preview.amount >= before.target.saturating_sub(before.for_spending));
    let op = core.confirm_refill(preview.id.clone()).await.unwrap();
    done(&events, op).await;

    let after = core.spending_state().await.unwrap();
    assert!(after.for_spending >= after.target);
    assert!(after.balance > before.balance);
    // Now at target: a second refill is refused, and the preview is spent.
    assert!(core.prepare_refill().await.is_err());
    assert!(core.confirm_refill(preview.id).await.is_err());
}
