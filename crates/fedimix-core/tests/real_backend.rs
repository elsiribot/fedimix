//! `RealBackend` over the scripted federation.
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fedimix_core::testing::{
    DepositEvent, FakeClient, FakePort, FederationClient, SendHalt, WatchUpdate, halt_sends,
    real_backend,
};
use fedimix_core::{
    Amount, Backend, ChangeDestination, CoreConfig, Event, EventStream, Features, FedimixError,
    HistoryItem, HistoryKind, HistoryStatus, Network, NoteBucket, Notice, OperationKind,
    OperationState, ReceiveMethod, ReceiveRequest, Result, RunningOperation, SendAmount,
    SendPreview, SendRequest, Settings, WalletKind,
};
use tempfile::TempDir;

const INVITE: &str = "fed1one";
const SECOND_INVITE: &str = "fed1two";
const RESTORING: &str = "Fedimix is still restoring your wallet. Try again when it has finished.";
const PROXY_SENTENCE: &str =
    "Fedimix cannot reach federations through a proxy yet. Clear the proxy in Settings to connect.";
/// Longer than anything here takes unless it is broken.
const PATIENCE: Duration = Duration::from_secs(10);

struct Harness {
    dir: TempDir,
    port: Arc<FakePort>,
    first: Arc<FakeClient>,
    second: Arc<FakeClient>,
    time: Arc<AtomicU64>,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let port = Arc::new(FakePort::default());
        let first = port.add_federation(INVITE, "one", Network::Regtest);
        let second = port.add_federation(SECOND_INVITE, "two", Network::Regtest);
        port.add_federation("fed1main", "main", Network::Bitcoin);
        port.add_federation_on("fed1unknown", "unknown", None);
        Self {
            dir,
            port,
            first,
            second,
            time: Arc::new(AtomicU64::new(1_000)),
        }
    }

    async fn open(&self) -> Result<Arc<fedimix_core::RealBackend>> {
        self.open_on(Network::Regtest).await
    }

    async fn open_on(&self, network: Network) -> Result<Arc<fedimix_core::RealBackend>> {
        let config = CoreConfig {
            data_dir: self.dir.path().to_str().unwrap().to_string(),
            network,
        };
        let time = Arc::clone(&self.time);
        let clock = Arc::new(move || time.load(Ordering::SeqCst));
        real_backend(config, self.port.clone(), clock).await
    }

    /// A backend that has joined the first federation.
    async fn joined(&self) -> Arc<fedimix_core::RealBackend> {
        let backend = self.open().await.unwrap();
        backend.join_federation(INVITE.into()).await.unwrap();
        wait_online(&*backend, true).await;
        backend
    }
}

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(PATIENCE, future).await {
        Ok(value) => value,
        Err(_) => panic!("timed out: {what}"),
    }
}

/// Polls `check` until it holds.
async fn wait_until<F: Future<Output = bool>>(what: &str, check: impl Fn() -> F) {
    within(what, async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

async fn wait_online(backend: &dyn Backend, online: bool) {
    wait_until("the federation's online state", || async {
        let state = backend.spending_state().await.unwrap();
        state.federation.is_some_and(|f| f.online == online)
    })
    .await;
}

async fn wait_balance(backend: &dyn Backend, balance: Amount) {
    wait_until("the balance", || async {
        backend.spending_state().await.unwrap().balance == balance
    })
    .await;
}

/// Reads events until one matches.
async fn wait_event(events: &EventStream, wanted: &Event) {
    within("the event", async {
        while events.next().await.as_ref() != Some(wanted) {}
    })
    .await;
}

fn with_proxy() -> Settings {
    Settings {
        socks5_proxy: Some("127.0.0.1:9050".into()),
        ..Settings::default()
    }
}

#[tokio::test]
async fn opens_with_an_unreachable_federation_and_recovers() {
    let harness = Harness::new();
    harness.first.set_balance(Amount::from_sat(4_000));
    let backend = harness.joined().await;
    wait_balance(&*backend, Amount::from_sat(4_000)).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.port.set_unreachable(true);
    let backend = harness.open().await.unwrap();
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.balance, Amount::from_sat(4_000));
    let federation = state.federation.unwrap();
    assert!(!federation.online);
    assert_eq!(federation.balance, Amount::from_sat(4_000));

    harness.first.set_balance(Amount::from_sat(5_000));
    harness.port.set_unreachable(false);
    wait_online(&*backend, true).await;
    wait_balance(&*backend, Amount::from_sat(5_000)).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn reads_do_not_wait_for_the_network() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.port.set_held(true);
    let backend = harness.open().await.unwrap();
    let quickly = Duration::from_secs(1);
    let state = tokio::time::timeout(quickly, backend.spending_state()).await;
    assert!(!state.unwrap().unwrap().federation.unwrap().online);
    let federations = tokio::time::timeout(quickly, backend.federations()).await;
    assert_eq!(federations.unwrap().unwrap().len(), 1);
    tokio::time::timeout(quickly, backend.queue_state())
        .await
        .unwrap()
        .unwrap();
    within("shutdown past a hanging open", backend.shutdown())
        .await
        .unwrap();
}

#[tokio::test]
async fn join_twice_is_idempotent_and_not_an_error() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let again = backend.join_federation(INVITE.into()).await.unwrap();
    assert_eq!(again.id, "one");
    assert_eq!(backend.federations().await.unwrap().len(), 1);
    assert_eq!(harness.port.joins(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn joins_at_the_same_time_join_once() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    harness.port.set_held(true);
    let (one, two) = (Arc::clone(&backend), Arc::clone(&backend));
    let first = tokio::spawn(async move { one.join_federation(INVITE.into()).await });
    let second = tokio::spawn(async move { two.join_federation(INVITE.into()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    harness.port.set_held(false);
    assert_eq!(first.await.unwrap().unwrap().id, "one");
    assert_eq!(second.await.unwrap().unwrap().id, "one");
    assert_eq!(harness.port.joins(), 1);
    assert_eq!(backend.federations().await.unwrap().len(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn wrong_network_federation_is_refused() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let reply = backend.join_federation("fed1main".into()).await;
    assert!(
        matches!(reply, Err(FedimixError::InvalidInvite(_))),
        "{reply:?}"
    );
    assert_eq!(harness.port.joins(), 0);
    assert!(backend.federations().await.unwrap().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_joined_stays_active() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    assert_eq!(backend.federations().await.unwrap().len(), 2);
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.federation.unwrap().id, "one");
    backend.shutdown().await.unwrap();
    drop(backend);

    let backend = harness.open().await.unwrap();
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.federation.unwrap().id, "one");
    assert_eq!(
        backend.set_active_federation("nope".into()).await,
        Err(FedimixError::UnknownItem("nope".into()))
    );
    backend.set_active_federation("two".into()).await.unwrap();
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.federation.unwrap().id, "two");
    backend.shutdown().await.unwrap();
}

const WRONG_PHRASE: &str = "This federation's data was made with a different recovery phrase.";

#[tokio::test]
async fn a_federation_that_cannot_open_says_why() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    assert_eq!(backend.federations().await.unwrap()[0].problem, None);
    backend.shutdown().await.unwrap();
    drop(backend);

    harness
        .port
        .set_open_error(Some(FedimixError::Internal(WRONG_PHRASE.into())));
    let backend = harness.open().await.unwrap();
    wait_until("the problem", || async {
        let state = backend.spending_state().await.unwrap();
        state.federation.unwrap().problem.as_deref() == Some(WRONG_PHRASE)
    })
    .await;
    backend.shutdown().await.unwrap();
    drop(backend);

    // Waiting cures Offline, so that is no problem to report.
    harness.port.set_open_error(Some(FedimixError::Offline));
    let backend = harness.open().await.unwrap();
    wait_until("the failed open", || async {
        harness.port.open_failures() > 1
    })
    .await;
    let federation = backend.spending_state().await.unwrap().federation.unwrap();
    assert_eq!(federation.problem, None);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn spending_state_splits_at_target() {
    let harness = Harness::new();
    harness.first.set_balance(Amount::from_sat(7_000_000));
    let backend = harness.joined().await;
    wait_balance(&*backend, Amount::from_sat(7_000_000)).await;
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.target, Amount::from_sat(5_000_000));
    assert_eq!(state.for_spending, Amount::from_sat(5_000_000));
    assert_eq!(state.moving, Amount::from_sat(2_000_000));
    assert!(!state.saving_ready);

    harness.first.set_balance(Amount::from_sat(3_000_000));
    wait_balance(&*backend, Amount::from_sat(3_000_000)).await;
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.for_spending, Amount::from_sat(3_000_000));
    assert_eq!(state.moving, Amount::ZERO);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn queue_waits_for_saving() {
    let harness = Harness::new();
    harness.first.set_balance(Amount::from_sat(7_000_000));
    let backend = harness.joined().await;
    wait_balance(&*backend, Amount::from_sat(7_000_000)).await;
    let queue = backend.queue_state().await.unwrap();
    assert!(queue.waiting_for_saving);
    assert!(queue.items.is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn notes_show_only_while_connected() {
    let harness = Harness::new();
    let notes = vec![NoteBucket {
        denomination: Amount::from_sat(1_024),
        count: 2,
    }];
    harness.first.set_notes(notes.clone());
    harness.first.set_balance(Amount::from_sat(2_048));
    let backend = harness.joined().await;
    wait_until("the notes", || async {
        backend.spending_state().await.unwrap().notes == notes
    })
    .await;

    harness.first.set_online(false);
    wait_online(&*backend, false).await;
    let state = backend.spending_state().await.unwrap();
    assert!(state.notes.is_empty());
    assert_eq!(state.balance, Amount::from_sat(2_048));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn proxy_set_means_no_connection_attempt() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.set_settings(with_proxy()).await.unwrap();
    wait_until("the client to close", || async {
        harness.first.shutdowns() == 1
    })
    .await;
    backend.shutdown().await.unwrap();
    drop(backend);

    let connects = harness.port.connects();
    let backend = harness.open().await.unwrap();
    assert_eq!(
        backend.join_federation(SECOND_INVITE.into()).await,
        Err(FedimixError::ProxyUnsupported)
    );
    assert_eq!(
        FedimixError::ProxyUnsupported.to_string(),
        PROXY_SENTENCE,
        "the UI shows this as it is"
    );
    let federations = backend.federations().await.unwrap();
    assert!(!federations[0].online);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.port.connects(), connects);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn setting_a_proxy_disconnects_and_clearing_it_connects() {
    let harness = Harness::new();
    let backend = harness.joined().await;

    backend.set_settings(with_proxy()).await.unwrap();
    assert!(
        !backend.federations().await.unwrap()[0].online,
        "offline from the moment the call returns"
    );
    wait_until("the client to close", || async {
        harness.first.shutdowns() == 1
    })
    .await;
    let connects = harness.port.connects();

    backend.set_settings(Settings::default()).await.unwrap();
    wait_online(&*backend, true).await;
    assert_eq!(harness.port.connects(), connects + 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn setting_a_proxy_cancels_a_join_in_progress() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    harness.port.set_held(true);
    let joining = Arc::clone(&backend);
    let join = tokio::spawn(async move { joining.join_federation(INVITE.into()).await });
    wait_until("the join to start", || async {
        harness.port.connects() > 0
    })
    .await;
    backend.set_settings(with_proxy()).await.unwrap();
    assert_eq!(
        within("the join to end", join).await.unwrap(),
        Err(FedimixError::ProxyUnsupported)
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn events_fire_on_balance_and_online_changes() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let events = backend.subscribe();
    backend.join_federation(INVITE.into()).await.unwrap();
    wait_event(&events, &Event::FederationChanged).await;
    wait_event(&events, &Event::ConnectionChanged { online: true }).await;

    let balance = Amount::from_sat(1_234);
    harness.first.set_balance(balance);
    wait_event(
        &events,
        &Event::BalanceChanged {
            wallet: WalletKind::Spending,
            balance,
        },
    )
    .await;
    harness.first.set_online(false);
    wait_event(&events, &Event::ConnectionChanged { online: false }).await;

    // Another federation is no change to Spending's own balance.
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    wait_event(&events, &Event::FederationChanged).await;
    harness.second.set_balance(Amount::from_sat(9));
    wait_event(&events, &Event::FederationChanged).await;
    wait_until("the second balance", || async {
        backend.federations().await.unwrap()[1].balance == Amount::from_sat(9)
    })
    .await;
    assert_eq!(backend.spending_state().await.unwrap().balance, balance);

    backend.set_settings(Settings::default()).await.unwrap();
    wait_event(&events, &Event::SettingsChanged).await;
    backend.shutdown().await.unwrap();
    assert_eq!(
        within("the stream to end", async {
            while events.next().await.is_some() {}
            None::<Event>
        })
        .await,
        None
    );
}

#[tokio::test]
async fn saving_methods_report_not_set_up() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let not_set_up = Err::<(), _>(FedimixError::NotSetUp(
        "Saving arrives in a later version.".into(),
    ));
    assert_eq!(backend.setup_saving("device".into()).await, not_set_up);
    assert_eq!(backend.set_coin_frozen("o".into(), true).await, not_set_up);
    assert_eq!(backend.prepare_refill().await.map(|_| ()), not_set_up);
    assert_eq!(
        backend.confirm_refill("p".into()).await.map(|_| ()),
        not_set_up
    );
    assert_eq!(
        backend.prepare_exit("i".into()).await.map(|_| ()),
        not_set_up
    );
    assert_eq!(backend.exit_now("p".into()).await.map(|_| ()), not_set_up);
    assert!(backend.detect_devices().await.unwrap().is_empty());
    assert!(backend.suggested_federations().await.unwrap().is_empty());
    let saving = backend.saving_state().await.unwrap();
    assert_eq!(saving.device, None);
    assert_eq!(saving.balance, Amount::ZERO);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_persist_and_ranges_are_checked() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    for bad in [
        Settings {
            anonymity_target: 0,
            ..Settings::default()
        },
        Settings {
            fee_guard_percent: 101,
            ..Settings::default()
        },
    ] {
        let reply = backend.set_settings(bad).await;
        assert!(
            matches!(reply, Err(FedimixError::InvalidAmount(_))),
            "{reply:?}"
        );
    }
    let changed = Settings {
        anonymity_target: 7,
        ..Settings::default()
    };
    backend.set_settings(changed.clone()).await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);
    let backend = harness.open().await.unwrap();
    assert_eq!(backend.settings().await.unwrap(), changed);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_words_stay_the_same_across_a_restart() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let words = backend.recovery_words().await.unwrap();
    assert_eq!(words.split(' ').count(), 12);
    backend.shutdown().await.unwrap();
    drop(backend);
    let backend = harness.open().await.unwrap();
    assert_eq!(backend.recovery_words().await.unwrap(), words);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_backend_on_one_folder_is_refused() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    assert_eq!(
        harness.open().await.err(),
        Some(FedimixError::Internal(
            "Fedimix is already running with this data folder.".into()
        ))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_panic_in_a_task_restarts_the_supervisor() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.first.panic_next_stream();
    harness.first.set_balance(Amount::from_sat(77));
    let backend = harness.open().await.unwrap();
    wait_online(&*backend, true).await;
    wait_balance(&*backend, Amount::from_sat(77)).await;
    assert_eq!(
        harness.first.shutdowns(),
        2,
        "the client of the panicked task was closed, so it could open again"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_client_whose_streams_end_is_opened_again() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let connects = harness.port.connects();
    // The stream ends with no word from the backend, as a closed client's does.
    let client: Arc<dyn FederationClient> = harness.first.clone();
    client.shutdown().await.unwrap();
    wait_online(&*backend, false).await;
    wait_online(&*backend, true).await;
    assert_eq!(harness.port.connects(), connects + 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_stops_every_task() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    within("both federations online", async {
        wait_until("both", || async {
            backend
                .federations()
                .await
                .unwrap()
                .iter()
                .all(|f| f.online)
        })
        .await;
    })
    .await;

    backend.shutdown().await.unwrap();
    assert_eq!(harness.first.shutdowns(), 1);
    assert_eq!(harness.second.shutdowns(), 1);
    backend.shutdown().await.unwrap();
    assert_eq!(harness.first.shutdowns(), 1, "shutdown is idempotent");

    // No task is left to notice this, and nothing reconnects.
    let connects = harness.port.connects();
    harness.first.set_balance(Amount::from_sat(1));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.port.connects(), connects);
    assert_eq!(
        backend.federations().await.unwrap()[0].balance,
        Amount::ZERO
    );
    assert_eq!(
        backend.join_federation(INVITE.into()).await,
        Err(FedimixError::Internal("Fedimix is shutting down.".into()))
    );

    // The lock went too: the folder opens again.
    let reopened = harness.open().await.unwrap();
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_the_backend_stops_its_tasks() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let events = backend.subscribe();
    drop(backend);
    wait_until("the client to close", || async {
        harness.first.shutdowns() == 1
    })
    .await;
    within("the stream to end", async {
        while events.next().await.is_some() {}
    })
    .await;
    // The stream ends as the last task lets go, and the lock goes with it.
    let reopened = harness.open().await.unwrap();
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn reopening_with_another_network_is_refused() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);

    let connects = harness.port.connects();
    let refused = harness.open_on(Network::Bitcoin).await;
    let error = refused.err().unwrap();
    assert_eq!(
        error,
        FedimixError::WrongNetwork {
            folder: "regtest".into(),
            wanted: "bitcoin".into(),
        }
    );
    assert_eq!(
        error.to_string(),
        "This data folder is for regtest, but Fedimix was started for bitcoin. Start it for regtest, or choose another data folder."
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(harness.port.connects(), connects, "no supervisor started");
    // The refusal let go of the folder, and did not change what it is for.
    let backend = harness.open().await.unwrap();
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_empty_folder_binds_on_first_open() {
    let harness = Harness::new();
    let backend = harness.open_on(Network::Signet).await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);
    assert!(matches!(
        harness.open().await.err(),
        Some(FedimixError::WrongNetwork { .. })
    ));
}

#[tokio::test]
async fn a_federation_of_unknown_network_is_refused_as_such() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    assert_eq!(
        backend.join_federation("fed1unknown".into()).await,
        Err(FedimixError::InvalidInvite(
            "Fedimix cannot tell which network this federation runs on.".into()
        ))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn joining_a_known_federation_needs_no_network() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    harness.port.set_unreachable(true);
    let connects = harness.port.connects();
    let again = backend.join_federation(INVITE.into()).await.unwrap();
    assert_eq!(again.id, "one");
    assert_eq!(harness.port.connects(), connects);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn join_reports_the_balance_of_an_existing_client_database() {
    let harness = Harness::new();
    harness.first.set_balance(Amount::from_sat(4_000));
    let backend = harness.open().await.unwrap();
    let joined = backend.join_federation(INVITE.into()).await.unwrap();
    assert_eq!(joined.balance, Amount::from_sat(4_000));
    assert_eq!(
        backend.federations().await.unwrap()[0].balance,
        Amount::from_sat(4_000)
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn join_retries_a_federation_that_will_not_open_at_once() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);

    let unreadable =
        FedimixError::Internal("Fedimix could not open this federation's data.".into());
    harness.port.set_open_error(Some(unreadable));
    let backend = harness.open().await.unwrap();
    wait_until("the open to fail", || async {
        harness.port.open_failures() > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    harness.port.set_open_error(None);
    let joins = harness.port.joins();
    harness.first.set_balance(Amount::from_sat(5));
    let joined = backend.join_federation(INVITE.into()).await.unwrap();
    assert_eq!(joined.id, "one");
    assert_eq!(harness.port.joins(), joins, "never a fresh start");
    assert_eq!(harness.port.recovers(), 0, "the data is there");
    assert_eq!(backend.federations().await.unwrap().len(), 1);
    // The supervisor takes it at once, not at the end of its backoff.
    tokio::time::timeout(Duration::from_millis(500), wait_online(&*backend, true))
        .await
        .unwrap();
    wait_balance(&*backend, Amount::from_sat(5)).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_offline_federation_is_not_rejoined() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.port.set_open_error(Some(FedimixError::Offline));
    let backend = harness.open().await.unwrap();
    wait_until("the open to fail", || async {
        harness.port.open_failures() > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let joins = harness.port.joins();
    backend.join_federation(INVITE.into()).await.unwrap();
    assert_eq!(harness.port.joins(), joins);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_cancelled_shutdown_leaves_its_work_to_the_next_call() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    harness.first.set_close_held(true);
    let brief = Duration::from_millis(50);
    assert!(
        tokio::time::timeout(brief, backend.shutdown())
            .await
            .is_err(),
        "the client is still closing"
    );
    assert!(
        tokio::time::timeout(brief, backend.shutdown())
            .await
            .is_err(),
        "a repeated call waits for the same work"
    );
    assert_eq!(
        harness.open().await.err(),
        Some(FedimixError::Internal(
            "Fedimix is already running with this data folder.".into()
        )),
        "the lock outlives the closing client"
    );
    harness.first.set_close_held(false);
    within("the shutdown", backend.shutdown()).await.unwrap();
    let reopened = harness.open().await.unwrap();
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_during_a_join_closes_the_joined_client() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    harness.first.set_balance_held(true);
    let joining = Arc::clone(&backend);
    let join = tokio::spawn(async move { joining.join_federation(INVITE.into()).await });
    wait_until("port.join to return", || async {
        harness.first.balance_reads() > 0
    })
    .await;
    let stopping = Arc::clone(&backend);
    let stop = tokio::spawn(async move { stopping.shutdown().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!stop.is_finished(), "shutdown waits for the join");
    harness.first.set_balance_held(false);
    within("the join", join).await.unwrap().unwrap();
    within("the shutdown", stop).await.unwrap().unwrap();
    assert_eq!(harness.first.shutdowns(), 1);
    let reopened = harness.open().await.unwrap();
    assert_eq!(reopened.federations().await.unwrap().len(), 1);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_join_the_store_refuses_closes_its_client() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let db = rusqlite::Connection::open(harness.dir.path().join("fedimix.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER no_rows BEFORE INSERT ON federations
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    let reply = backend.join_federation(INVITE.into()).await;
    assert!(matches!(reply, Err(FedimixError::Internal(_))), "{reply:?}");
    assert_eq!(harness.first.shutdowns(), 1);
    assert!(backend.federations().await.unwrap().is_empty());
    backend.shutdown().await.unwrap();
}

/// Seconds between one attempt and the next.
fn gaps(port: &FakePort) -> Vec<u64> {
    let attempts = port.attempts();
    attempts
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).as_secs())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn attempts_back_off_to_a_minute() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    harness.port.set_unreachable(true);
    let before = harness.port.attempts().len();
    let backend = harness.open().await.unwrap();
    tokio::time::sleep(Duration::from_secs(300)).await;
    let attempts = harness.port.attempts()[before..].to_vec();
    let seconds: Vec<u64> = attempts
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).as_secs())
        .collect();
    assert_eq!(seconds[..8], [1, 2, 4, 8, 16, 32, 60, 60], "{seconds:?}");
    backend.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn only_a_lasting_connection_resets_the_backoff() {
    let harness = Harness::new();
    let port = &harness.port;
    let backend = harness.joined().await;
    let client: Arc<dyn FederationClient> = harness.first.clone();

    // A connection that ends at once: attempts keep backing off.
    client.clone().shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    client.clone().shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let short = gaps(port);
    assert_eq!(short[short.len() - 1], 2, "{short:?}");

    // One that lasted a minute starts over.
    tokio::time::sleep(Duration::from_secs(90)).await;
    client.clone().shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let attempts = port.attempts();
    assert_eq!(attempts.len(), short.len() + 2, "{:?}", gaps(port));
    backend.shutdown().await.unwrap();
}

const FIRST_FEDERATION: &str = "one";

fn invoice_request(sat: u64) -> ReceiveRequest {
    ReceiveRequest {
        wallet: WalletKind::Spending,
        method: ReceiveMethod::Lightning {
            amount: Amount::from_sat(sat),
            description: "test".into(),
            expiry_secs: 600,
        },
    }
}

fn address_request() -> ReceiveRequest {
    ReceiveRequest {
        wallet: WalletKind::Spending,
        method: ReceiveMethod::OnChain,
    }
}

fn settled(sat: u64) -> WatchUpdate {
    WatchUpdate::Settled {
        amount: Amount::from_sat(sat),
        fee: Amount::from_sat(1),
        txid: None,
    }
}

fn confirming(confirmations: u32, sat: u64) -> WatchUpdate {
    WatchUpdate::Confirming {
        confirmations,
        required: 6,
        amount: Amount::from_sat(sat),
    }
}

fn deposit_event(cursor: u64, id: &str, confirmed: bool) -> DepositEvent {
    DepositEvent {
        cursor,
        id: id.into(),
        amount: Amount::from_sat(40_000),
        fee: Amount::from_sat(100),
        txid: Some("txid".into()),
        confirmed,
    }
}

/// The watch id `FakeClient` gave a payload: what follows its first `-` of two.
fn watch_id(payload: &str) -> &str {
    payload.split_once('-').unwrap().1
}

async fn rows(backend: &dyn Backend) -> Vec<HistoryItem> {
    let page = backend.history(WalletKind::Spending, None, 100).await;
    page.unwrap().items
}

async fn wait_status(backend: &dyn Backend, id: &str, status: HistoryStatus) {
    wait_until("the history row", || async {
        rows(backend)
            .await
            .iter()
            .any(|row| row.id == id && row.status == status)
    })
    .await;
}

/// What arrives within a moment, for tests of what must not.
async fn drain(events: &EventStream) -> Vec<Event> {
    let mut seen = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(50), events.next()).await
    {
        seen.push(event);
    }
    seen
}

fn notices(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::Notification(_)))
        .count()
}

/// Opens `fedimix.db` beside a backend, for what the API cannot show.
fn raw_store(harness: &Harness) -> rusqlite::Connection {
    rusqlite::Connection::open(harness.dir.path().join("fedimix.db")).unwrap()
}

#[tokio::test]
async fn invoice_settles_into_history() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let events = backend.subscribe();

    let info = backend.receive(invoice_request(5_000)).await.unwrap();
    assert_eq!(info.uri, format!("lightning:{}", info.payload));
    assert_eq!(info.expires_at, Some(1_600));
    assert_eq!(info.min_deposit, None);
    let id = watch_id(&info.payload).to_string();
    let row = rows(&*backend)
        .await
        .pop()
        .expect("the row exists on return");
    assert_eq!(row.id, id);
    assert_eq!(row.kind, HistoryKind::ReceivedLightning);
    assert_eq!(row.status, HistoryStatus::Pending);
    assert_eq!(row.amount, Amount::from_sat(5_000));
    assert!(row.incoming);
    assert_eq!(row.timestamp, 1_000);

    harness.first.push_update(&id, settled(4_990));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    let row = rows(&*backend).await.pop().unwrap();
    assert_eq!(row.amount, Amount::from_sat(4_990));
    wait_event(
        &events,
        &Event::Notification(Notice::DepositConfirmed {
            amount: Amount::from_sat(4_990),
        }),
    )
    .await;
    backend.shutdown().await.unwrap();
    assert_eq!(watches_left(&harness), 0);
}

fn watches_left(harness: &Harness) -> i64 {
    raw_store(harness)
        .query_row("SELECT COUNT(*) FROM watches WHERE finished = 0", [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[tokio::test]
async fn unpaid_invoice_expires() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let events = backend.subscribe();
    let info = backend.receive(invoice_request(5_000)).await.unwrap();
    let id = watch_id(&info.payload).to_string();
    harness.first.push_update(&id, WatchUpdate::Expired);
    wait_status(&*backend, &id, HistoryStatus::Expired).await;
    assert_eq!(notices(&drain(&events).await), 0);
    backend.shutdown().await.unwrap();
    assert_eq!(watches_left(&harness), 0);
}

#[tokio::test]
async fn deposit_walks_through_confirming() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let events = backend.subscribe();
    let info = backend.receive(address_request()).await.unwrap();
    assert_eq!(info.uri, format!("bitcoin:{}", info.payload));
    assert_eq!(info.expires_at, None);
    assert!(
        rows(&*backend).await.is_empty(),
        "an address alone adds no row"
    );
    let id = watch_id(&info.payload).to_string();

    harness.first.push_update(&id, confirming(1, 40_000));
    let at_one = HistoryStatus::Confirming {
        confirmations: 1,
        required: 6,
    };
    wait_status(&*backend, &id, at_one).await;
    let row = rows(&*backend).await.pop().unwrap();
    assert_eq!(row.kind, HistoryKind::Deposit);
    assert_eq!(row.amount, Amount::from_sat(40_000));

    harness.first.push_update(&id, settled(39_900));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    wait_event(
        &events,
        &Event::Notification(Notice::DepositConfirmed {
            amount: Amount::from_sat(39_900),
        }),
    )
    .await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_resumes_watches() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let invoice = backend.receive(invoice_request(5_000)).await.unwrap();
    let address = backend.receive(address_request()).await.unwrap();
    let (invoice, address) = (watch_id(&invoice.payload), watch_id(&address.payload));
    harness.first.push_update(address, confirming(1, 40_000));
    wait_until("the deposit's row", || async {
        rows(&*backend).await.len() == 2
    })
    .await;
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.first.push_update(invoice, settled(4_990));
    harness.first.push_update(address, settled(39_900));
    let backend = harness.open().await.unwrap();
    wait_status(&*backend, invoice, HistoryStatus::Complete).await;
    wait_status(&*backend, address, HistoryStatus::Complete).await;
    backend.shutdown().await.unwrap();
    assert_eq!(watches_left(&harness), 0);
}

#[tokio::test]
async fn a_watch_stream_that_ends_keeps_its_watch() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let info = backend.receive(invoice_request(5_000)).await.unwrap();
    let id = watch_id(&info.payload).to_string();
    // Every stream ends with no final update, as a closed client's do.
    let client: Arc<dyn FederationClient> = harness.first.clone();
    client.shutdown().await.unwrap();
    wait_online(&*backend, false).await;
    assert_eq!(rows(&*backend).await[0].status, HistoryStatus::Pending);
    assert_eq!(watches_left(&harness), 1);

    wait_online(&*backend, true).await;
    harness.first.push_update(&id, settled(4_990));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    backend.shutdown().await.unwrap();
    assert_eq!(watches_left(&harness), 0);
}

#[tokio::test]
async fn a_replayed_completion_notifies_nobody() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let events = backend.subscribe();
    let info = backend.receive(address_request()).await.unwrap();
    let id = watch_id(&info.payload).to_string();
    harness.first.push_update(&id, confirming(1, 40_000));
    harness.first.push_update(&id, settled(39_900));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    backend.shutdown().await.unwrap();
    assert_eq!(notices(&drain(&events).await), 1);
    drop(backend);

    // A crash between writing the row and finishing the watch leaves this.
    raw_store(&harness)
        .execute("UPDATE watches SET finished = 0", [])
        .unwrap();
    let backend = harness.open().await.unwrap();
    let events = backend.subscribe();
    wait_until("the replay to end", || async {
        watches_left(&harness) == 0
    })
    .await;
    assert_eq!(rows(&*backend).await[0].status, HistoryStatus::Complete);
    let seen = drain(&events).await;
    assert_eq!(notices(&seen), 0, "{seen:?}");
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn v2_deposits_reach_history_and_notify_once() {
    let harness = Harness::new();
    harness.first.set_deposit_watches(false);
    let backend = harness.joined().await;
    let events = backend.subscribe();
    let info = backend.receive(address_request()).await.unwrap();
    assert!(
        rows(&*backend).await.is_empty(),
        "an address alone adds no row"
    );
    assert_eq!(watches_left(&harness), 0, "this generation has no watch");

    harness
        .first
        .push_deposit(deposit_event(1, "outpoint", false));
    wait_status(&*backend, "outpoint", HistoryStatus::Pending).await;
    harness
        .first
        .push_deposit(deposit_event(2, "outpoint", true));
    wait_status(&*backend, "outpoint", HistoryStatus::Complete).await;
    let row = rows(&*backend).await.pop().unwrap();
    assert_eq!(row.kind, HistoryKind::Deposit);
    assert_eq!(row.amount, Amount::from_sat(40_000));
    let seen = drain(&events).await;
    assert_eq!(notices(&seen), 1, "{seen:?}");
    assert!(info.uri.starts_with("bitcoin:"));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn v2_cursor_is_not_replayed_after_restart() {
    let harness = Harness::new();
    harness.first.set_deposit_watches(false);
    let backend = harness.joined().await;
    harness.first.push_deposit(deposit_event(1, "first", true));
    wait_status(&*backend, "first", HistoryStatus::Complete).await;
    backend.shutdown().await.unwrap();
    drop(backend);
    let cursor: i64 = raw_store(&harness)
        .query_row(
            "SELECT value FROM cursors WHERE name = 'deposits:one'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor, 1);

    // A row the log would rewrite if it were read again.
    raw_store(&harness)
        .execute("DELETE FROM history", [])
        .unwrap();
    harness.first.push_deposit(deposit_event(2, "second", true));
    let backend = harness.open().await.unwrap();
    wait_status(&*backend, "second", HistoryStatus::Complete).await;
    assert_eq!(rows(&*backend).await.len(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn deposit_at_or_below_minimum_is_stated() {
    let harness = Harness::new();
    harness.first.set_min_deposit(Ok(Amount::from_sat(1_234)));
    let backend = harness.joined().await;
    let events = backend.subscribe();
    let info = backend.receive(address_request()).await.unwrap();
    assert_eq!(info.min_deposit, Some(Amount::from_sat(1_234)));

    // The generation with watches reports a deposit it did not credit.
    let id = watch_id(&info.payload).to_string();
    harness.first.push_update(&id, confirming(0, 1_000));
    let reason =
        "This deposit was too small to cover the federation's fee, so it was not credited.";
    harness.first.push_update(
        &id,
        WatchUpdate::Failed {
            reason: reason.into(),
        },
    );
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    assert_eq!(rows(&*backend).await[0].amount, Amount::from_sat(1_000));
    assert_eq!(notices(&drain(&events).await), 0);
    backend.shutdown().await.unwrap();
    assert_eq!(watches_left(&harness), 0);
}

#[tokio::test]
async fn a_deposit_watch_failing_with_no_deposit_adds_no_row() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let info = backend.receive(address_request()).await.unwrap();
    let id = watch_id(&info.payload).to_string();
    harness.first.push_update(
        &id,
        WatchUpdate::Failed {
            reason: "Fedimix has no record of this operation.".into(),
        },
    );
    wait_until("the watch to finish", || async {
        watches_left(&harness) == 0
    })
    .await;
    assert!(rows(&*backend).await.is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn receive_while_offline_is_offline() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    harness.first.set_online(false);
    wait_online(&*backend, false).await;
    assert_eq!(
        backend.receive(invoice_request(1)).await,
        Err(FedimixError::Offline)
    );
    assert_eq!(
        backend.receive(address_request()).await,
        Err(FedimixError::Offline)
    );
    assert!(rows(&*backend).await.is_empty());
    backend.shutdown().await.unwrap();
    assert_eq!(watches_left(&harness), 0);
}

#[tokio::test]
async fn no_lightning_module_is_a_sentence() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let error = FedimixError::NotSetUp("This federation does not support Lightning.".into());
    harness.first.fail_next_invoice(error.clone());
    assert_eq!(backend.receive(invoice_request(1)).await, Err(error));
    assert!(rows(&*backend).await.is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn receive_needs_spending_and_a_federation() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let saving = ReceiveRequest {
        wallet: WalletKind::Saving,
        method: ReceiveMethod::OnChain,
    };
    assert_eq!(
        backend.receive(saving).await,
        Err(FedimixError::NotSetUp(
            "Saving arrives in a later version.".into()
        ))
    );
    assert_eq!(
        backend.receive(address_request()).await,
        Err(FedimixError::NotSetUp(
            "Join a federation before receiving.".into()
        ))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn unused_deposit_watches_stop_at_the_cap() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let first = backend.receive(address_request()).await.unwrap();
    let first = watch_id(&first.payload).to_string();
    let second = backend.receive(address_request()).await.unwrap();
    let second = watch_id(&second.payload).to_string();
    // A deposit on its way must not lose its watch, however old.
    harness.first.push_update(&first, confirming(0, 40_000));
    wait_until("the deposit's row", || async {
        rows(&*backend).await.len() == 1
    })
    .await;
    for _ in 0..100 {
        backend.receive(address_request()).await.unwrap();
    }
    backend.shutdown().await.unwrap();
    assert_eq!(watches_left(&harness), 101, "100 unused, and the one paid");
    let kept = |id: &str| {
        raw_store(&harness)
            .query_row(
                "SELECT finished = 0 FROM watches WHERE id = ?1",
                [id],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
    };
    assert!(kept(&first));
    assert!(!kept(&second), "the oldest address nobody paid went");
}

/// A backend whose first federation has lost its client database.
async fn lost_client_database(harness: &Harness) -> Arc<fedimix_core::RealBackend> {
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    harness
        .port
        .set_open_error(Some(FedimixError::UnknownItem(FIRST_FEDERATION.into())));
    let backend = harness.open().await.unwrap();
    wait_until("the recovery to start", || async {
        harness.port.recovers() > 0
    })
    .await;
    harness.port.set_open_error(None);
    backend
}

#[tokio::test]
async fn a_lost_client_database_recovers_in_the_background() {
    let harness = Harness::new();
    harness.first.set_balance(Amount::from_sat(4_000));
    harness.port.set_recover_held(true);
    let joins = {
        let backend = lost_client_database(&harness).await;
        let quickly = Duration::from_secs(1);
        let state = tokio::time::timeout(quickly, backend.spending_state()).await;
        assert!(!state.unwrap().unwrap().federation.unwrap().online);
        assert_eq!(
            backend.receive(invoice_request(1)).await,
            Err(FedimixError::Internal(RESTORING.into()))
        );

        harness.port.set_recover_held(false);
        wait_online(&*backend, true).await;
        wait_balance(&*backend, Amount::from_sat(4_000)).await;
        backend.shutdown().await.unwrap();
        harness.port.joins()
    };
    assert_eq!(joins, 1, "only the first join, never a fresh start");
    assert_eq!(harness.port.recovers(), 1);
}

async fn recovering(backend: &fedimix_core::RealBackend) -> bool {
    backend.federations().await.unwrap()[0].recovering
}

#[tokio::test]
async fn a_recovery_is_reported_until_the_client_is_usable() {
    let harness = Harness::new();
    harness.port.set_recover_held(true);
    let backend = lost_client_database(&harness).await;
    wait_until("the mark", || async { recovering(&backend).await }).await;
    let state = backend.spending_state().await.unwrap();
    assert!(state.federation.unwrap().recovering);

    harness.port.set_recover_held(false);
    wait_online(&*backend, true).await;
    assert!(!recovering(&backend).await);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn sending_and_receiving_during_a_recovery_say_so() {
    let harness = Harness::new();
    harness.port.set_recover_held(true);
    let backend = lost_client_database(&harness).await;
    wait_until("the mark", || async { recovering(&backend).await }).await;
    let restoring = FedimixError::Internal(RESTORING.into());
    assert_eq!(
        backend.receive(invoice_request(1)).await.err().as_ref(),
        Some(&restoring)
    );
    let send = SendRequest {
        wallet: WalletKind::Spending,
        destination: "lnbc1000n1invoice".into(),
        amount: SendAmount::FromInvoice,
        fee_rate_sat_vb: None,
    };
    assert_eq!(
        backend.prepare_send(send).await.err().as_ref(),
        Some(&restoring)
    );

    // Behind a proxy nothing runs, so the proxy is what the user must hear.
    backend.set_settings(with_proxy()).await.unwrap();
    assert_ne!(
        backend.receive(invoice_request(1)).await.err().as_ref(),
        Some(&restoring)
    );
    harness.port.set_recover_held(false);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_normal_connection_never_reports_a_recovery() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    assert!(!recovering(&backend).await);
    backend.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failed_recovery_still_reports_recovering_while_it_retries() {
    let harness = Harness::new();
    harness
        .port
        .set_recover_error(Some(FedimixError::Internal("It failed.".into())));
    let backend = lost_client_database(&harness).await;
    harness
        .port
        .set_open_error(Some(FedimixError::UnknownItem(FIRST_FEDERATION.into())));
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(harness.port.recovers() > 1);
    assert!(recovering(&backend).await);
    harness.port.set_recover_error(None);
    harness.port.set_open_error(None);
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert!(!recovering(&backend).await);
    backend.shutdown().await.unwrap();
}

/// A restart cut the recovery off, and `open` resumes it with no sign that it is
/// one: only what the store remembers can say so.
#[tokio::test]
async fn a_recovery_cut_off_by_a_restart_is_still_reported() {
    let harness = Harness::new();
    harness.port.set_recover_held(true);
    let backend = lost_client_database(&harness).await;
    wait_until("the mark", || async { recovering(&backend).await }).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.port.set_held(true);
    let backend = harness.open().await.unwrap();
    wait_until("the report", || async { recovering(&backend).await }).await;
    harness.port.set_recover_held(false);
    harness.port.set_held(false);
    wait_online(&*backend, true).await;
    assert!(!recovering(&backend).await);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_proxy_shows_offline_rather_than_a_recovery_that_cannot_run() {
    let harness = Harness::new();
    harness.port.set_recover_held(true);
    let backend = lost_client_database(&harness).await;
    wait_until("the mark", || async { recovering(&backend).await }).await;
    backend.set_settings(with_proxy()).await.unwrap();
    assert!(!recovering(&backend).await);
    harness.port.set_recover_held(false);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_real_backend_offers_no_later_features() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.features, Features::NONE);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_older_settings_copy_does_not_bring_the_backup_prompt_back() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let older = backend.settings().await.unwrap();
    backend
        .set_settings(Settings {
            recovery_words_shown: true,
            ..older.clone()
        })
        .await
        .unwrap();
    backend
        .set_settings(Settings {
            anonymity_target: 7,
            ..older
        })
        .await
        .unwrap();
    let now = backend.settings().await.unwrap();
    assert!(now.recovery_words_shown);
    assert_eq!(now.anonymity_target, 7);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_cut_off_recovery_starts_again_on_the_next_connect() {
    let harness = Harness::new();
    harness.port.set_recover_held(true);
    let backend = lost_client_database(&harness).await;
    // A proxy aborts the connection task in the middle of the recovery.
    harness
        .port
        .set_open_error(Some(FedimixError::UnknownItem(FIRST_FEDERATION.into())));
    backend.set_settings(with_proxy()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    harness.port.set_recover_held(false);
    backend.set_settings(Settings::default()).await.unwrap();
    wait_until("a second recovery", || async {
        harness.port.recovers() == 2
    })
    .await;
    harness.port.set_open_error(None);
    wait_online(&*backend, true).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failed_recovery_is_tried_again() {
    let harness = Harness::new();
    harness
        .port
        .set_recover_error(Some(FedimixError::Internal("It failed.".into())));
    let backend = lost_client_database(&harness).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!backend.federations().await.unwrap()[0].online);
    harness
        .port
        .set_open_error(Some(FedimixError::UnknownItem(FIRST_FEDERATION.into())));
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(harness.port.recovers() > 1);
    harness.port.set_recover_error(None);
    harness.port.set_open_error(None);
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert!(backend.federations().await.unwrap()[0].online);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_reads_the_new_deposit_log_from_the_start() {
    let harness = Harness::new();
    harness.first.set_deposit_watches(false);
    let backend = harness.joined().await;
    harness.first.push_deposit(deposit_event(1, "old", true));
    harness.first.push_deposit(deposit_event(2, "older", true));
    wait_status(&*backend, "older", HistoryStatus::Complete).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    // The recovered client starts an empty log; position 2 means nothing in it.
    harness.first.lose_deposit_log();
    harness.first.push_deposit(deposit_event(1, "new", true));
    harness
        .port
        .set_open_error(Some(FedimixError::UnknownItem(FIRST_FEDERATION.into())));
    let backend = harness.open().await.unwrap();
    wait_until("the recovery", || async { harness.port.recovers() > 0 }).await;
    harness.port.set_open_error(None);
    wait_status(&*backend, "new", HistoryStatus::Complete).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_capped_watch_that_sees_a_deposit_is_kept() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let oldest = backend.receive(address_request()).await.unwrap();
    let oldest = watch_id(&oldest.payload).to_string();
    for _ in 0..100 {
        backend.receive(address_request()).await.unwrap();
    }
    assert_eq!(watches_left(&harness), 100, "the oldest went past the cap");

    // Its task runs on until the connection ends, and the address is paid.
    harness.first.push_update(&oldest, confirming(1, 40_000));
    wait_until("the deposit's row", || async {
        rows(&*backend).await.len() == 1
    })
    .await;
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.first.push_update(&oldest, settled(39_900));
    let backend = harness.open().await.unwrap();
    wait_status(&*backend, &oldest, HistoryStatus::Complete).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_known_outpoint_in_the_new_log_stays_silent() {
    let harness = Harness::new();
    harness.first.set_deposit_watches(false);
    let backend = harness.joined().await;
    let events = backend.subscribe();
    harness
        .first
        .push_deposit(deposit_event(1, "outpoint", true));
    wait_status(&*backend, "outpoint", HistoryStatus::Complete).await;
    assert_eq!(notices(&drain(&events).await), 1);
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.first.lose_deposit_log();
    harness
        .first
        .push_deposit(deposit_event(1, "outpoint", false));
    harness
        .first
        .push_deposit(deposit_event(2, "outpoint", true));
    harness
        .port
        .set_open_error(Some(FedimixError::UnknownItem(FIRST_FEDERATION.into())));
    let backend = harness.open().await.unwrap();
    let events = backend.subscribe();
    wait_until("the recovery", || async { harness.port.recovers() > 0 }).await;
    harness.port.set_open_error(None);
    wait_until("the log to be read", || async {
        let cursor: i64 = raw_store(&harness)
            .query_row(
                "SELECT value FROM cursors WHERE name = 'deposits:one'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        cursor == 2
    })
    .await;
    let seen = drain(&events).await;
    assert_eq!(notices(&seen), 0, "{seen:?}");
    let rows = rows(&*backend).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, HistoryStatus::Complete);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_invoice_expires_counted_from_the_request() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    harness.first.set_invoice_held(true);
    let asking = Arc::clone(&backend);
    let asked = tokio::spawn(async move { asking.receive(invoice_request(1)).await });
    wait_until("the gateway call", || async {
        harness.first.invoice_calls() == 1
    })
    .await;
    harness.time.store(1_020, Ordering::SeqCst);
    harness.first.set_invoice_held(false);
    let info = within("the invoice", asked).await.unwrap().unwrap();
    assert_eq!(info.expires_at, Some(1_600));
    backend.shutdown().await.unwrap();
}

/// Removes the store and leaves the seed, as losing one file would.
fn lose_store(harness: &Harness) {
    for name in ["fedimix.db", "fedimix.db-wal", "fedimix.db-shm"] {
        let _ = std::fs::remove_file(harness.dir.path().join(name));
    }
}

#[tokio::test]
async fn a_lost_store_with_a_living_seed_recovers_on_join() {
    let harness = Harness::new();
    harness.first.set_balance(Amount::from_sat(4_000));
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    lose_store(&harness);
    harness
        .port
        .set_open_error(Some(FedimixError::UnknownItem(FIRST_FEDERATION.into())));

    let backend = harness.open().await.unwrap();
    let joins = harness.port.joins();
    harness.port.set_recover_held(true);
    let joined = within("the join", backend.join_federation(INVITE.into()))
        .await
        .unwrap();
    assert_eq!(joined.id, "one");
    assert!(!joined.online, "offline until the recovery ends");
    wait_until("the recovery", || async { harness.port.recovers() == 1 }).await;
    harness.port.set_open_error(None);
    harness.port.set_recover_held(false);
    wait_online(&*backend, true).await;
    wait_balance(&*backend, Amount::from_sat(4_000)).await;
    assert_eq!(harness.port.joins(), joins, "never a fresh start");
    backend.shutdown().await.unwrap();
    drop(backend);

    // Remembered: the seed is still older than this store after a restart.
    let backend = harness.open().await.unwrap();
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    assert_eq!(harness.port.joins(), joins);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_seed_born_with_its_store_joins_fresh_also_after_a_restart() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    assert_eq!(harness.port.joins(), 1);
    backend.shutdown().await.unwrap();
    drop(backend);

    let backend = harness.open().await.unwrap();
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    assert_eq!(harness.port.joins(), 2);
    assert_eq!(harness.port.recovers(), 0);
    backend.shutdown().await.unwrap();
}

/// What refusing a folder whose seed is gone must look like.
async fn assert_refused_for_a_missing_seed(harness: &Harness) {
    let seed = harness.dir.path().join("seed");
    std::fs::remove_file(&seed).unwrap();
    let reply = harness.open().await;
    let Err(FedimixError::Internal(sentence)) = &reply else {
        panic!("started without its seed: {:?}", reply.map(|_| ()));
    };
    assert!(sentence.contains(seed.to_str().unwrap()), "{sentence}");
    assert!(sentence.contains("Restore"), "{sentence}");
    assert!(!seed.exists(), "no new seed may take the old one's place");
}

#[tokio::test]
async fn a_missing_seed_beside_a_joined_store_is_refused_not_replaced() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    let words = std::fs::read_to_string(harness.dir.path().join("seed")).unwrap();

    assert_refused_for_a_missing_seed(&harness).await;

    // The file coming back is all it takes.
    let seed = harness.dir.path().join("seed");
    std::fs::write(&seed, &words).unwrap();
    std::fs::set_permissions(&seed, std::fs::Permissions::from_mode(0o600)).unwrap();
    let backend = harness.open().await.unwrap();
    assert_eq!(backend.recovery_words().await.unwrap(), words);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_missing_seed_beside_a_store_with_no_federation_is_refused() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);
    assert_refused_for_a_missing_seed(&harness).await;
}

#[tokio::test]
async fn a_lost_seed_and_store_start_a_new_wallet() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);
    lose_store(&harness);
    std::fs::remove_file(harness.dir.path().join("seed")).unwrap();
    let backend = harness.open().await.unwrap();
    backend.shutdown().await.unwrap();
}

/// 200 sat. The fake needs `add_invoice` to know one; this one is lexically real.
const INVOICE: &str = "lnbcrt2000n1abc";
const ADDRESS: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
const MAINNET_ADDRESS: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
const LN_FEE: Amount = Amount { msat: 10_000 };
const CAME_BACK: &str = "The payment failed and the money came back.";
const NEVER_SENT: &str = "This payment was never sent, so no money left your wallet.";

fn sat(n: u64) -> Amount {
    Amount::from_sat(n)
}

fn to(destination: &str, amount: SendAmount) -> SendRequest {
    SendRequest {
        wallet: WalletKind::Spending,
        destination: destination.into(),
        amount,
        fee_rate_sat_vb: None,
    }
}

fn pay_request() -> SendRequest {
    to(INVOICE, SendAmount::FromInvoice)
}

/// A backend on the first federation with `balance`, `INVOICE` payable.
async fn funded(harness: &Harness, balance: Amount) -> Arc<fedimix_core::RealBackend> {
    harness.first.set_balance(balance);
    harness.first.add_invoice(INVOICE, sat(200));
    let backend = harness.joined().await;
    wait_balance(&*backend, balance).await;
    backend
}

/// The states of operation `id` to its final one, and that one's `history_id`.
async fn run_of(events: &EventStream, id: &str) -> (Vec<OperationState>, Option<String>) {
    within("the operation's final state", async {
        let mut states = Vec::new();
        loop {
            let Some(Event::Operation {
                id: seen,
                kind,
                state,
                history_id,
            }) = events.next().await
            else {
                continue;
            };
            if seen != id {
                continue;
            }
            assert_eq!(kind, OperationKind::Send);
            let done = state.is_final();
            states.push(state);
            if done {
                return (states, history_id);
            }
            assert_eq!(history_id, None, "no row is reported before the end");
        }
    })
    .await
}

fn failed_with(sentence: &str) -> OperationState {
    OperationState::Failed {
        error: FedimixError::Internal(sentence.into()),
    }
}

/// The fake's operation behind intent `id`, once it has been submitted.
async fn operation_of(client: &FakeClient, id: &str) -> String {
    wait_until("the submission", || async {
        client.find_operation(id).await.unwrap().is_some()
    })
    .await;
    client.find_operation(id).await.unwrap().unwrap()
}

fn row_of(rows: &[HistoryItem], id: &str) -> HistoryItem {
    let row = rows.iter().find(|row| row.id == id);
    row.expect("the send has a history row").clone()
}

async fn restarted(
    harness: &Harness,
    backend: Arc<fedimix_core::RealBackend>,
) -> Arc<fedimix_core::RealBackend> {
    backend.shutdown().await.unwrap();
    drop(backend);
    harness.open().await.unwrap()
}

#[tokio::test]
async fn preview_pays_once() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let events = backend.subscribe();
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    assert_eq!(preview.wallet, WalletKind::Spending);
    assert_eq!(preview.destination, INVOICE);
    assert_eq!(preview.amount, sat(200));
    assert_eq!(preview.fee, LN_FEE);
    assert_eq!(preview.inputs, Vec::new());
    assert_eq!(preview.change, ChangeDestination::None);
    assert_eq!(preview.expires_at, 1_600);

    let id = backend.confirm_send(preview.id.clone()).await.unwrap();
    let again = backend.confirm_send(preview.id.clone()).await;
    assert_eq!(again, Err(FedimixError::UnknownItem(preview.id)));
    let op = operation_of(&harness.first, &id).await;
    assert_eq!(harness.first.paid(), [INVOICE]);
    wait_until("the running operation", || async {
        backend.operations().await.unwrap()
            == vec![RunningOperation {
                id: id.clone(),
                kind: OperationKind::Send,
                state: OperationState::Broadcasting,
            }]
    })
    .await;
    backend.cancel_operation(id.clone()).await.unwrap();
    let row = row_of(&rows(&*backend).await, &id);
    assert_eq!(row.kind, HistoryKind::Paid);
    assert_eq!(row.amount, sat(200));
    assert!(!row.incoming);
    assert_eq!(row.status, HistoryStatus::Pending);

    harness.first.push_update(&op, settled(200));
    let (states, history_id) = run_of(&events, &id).await;
    assert_eq!(states, [OperationState::Broadcasting, OperationState::Done]);
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    assert!(backend.operations().await.unwrap().is_empty());
    backend.cancel_operation(id).await.unwrap();
    assert_eq!(harness.first.paid(), [INVOICE]);
    assert_eq!(harness.first.pay_calls(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn confirming_one_preview_at_the_same_time_pays_once() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let (one, two) = (Arc::clone(&backend), Arc::clone(&backend));
    let (first_id, second_id) = (preview.id.clone(), preview.id);
    let first = tokio::spawn(async move { one.confirm_send(first_id).await });
    let second = tokio::spawn(async move { two.confirm_send(second_id).await });
    let replies = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(replies.iter().filter(|reply| reply.is_ok()).count(), 1);
    wait_until("the payment", || async { harness.first.paid().len() == 1 }).await;
    assert_eq!(rows(&*backend).await.len(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_preview_refused() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    harness.time.store(preview.expires_at, Ordering::SeqCst);
    assert_eq!(
        backend.confirm_send(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
    assert_eq!(harness.first.pay_calls(), 0);
    assert!(rows(&*backend).await.is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn discarded_preview_is_gone() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    backend.discard_preview(preview.id.clone()).await.unwrap();
    backend.discard_preview(preview.id.clone()).await.unwrap();
    assert_eq!(
        backend.confirm_send(preview.id.clone()).await,
        Err(FedimixError::UnknownItem(preview.id))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_previews_over_the_same_funds_second_fails() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(300)).await;
    harness.first.add_invoice("lnbcrt2000n1abd", sat(200));
    let first = backend.prepare_send(pay_request()).await.unwrap();
    let second = backend
        .prepare_send(to("lnbcrt2000n1abd", SendAmount::FromInvoice))
        .await
        .unwrap();
    backend.confirm_send(first.id).await.unwrap();
    let reply = backend.confirm_send(second.id).await;
    assert!(
        matches!(reply, Err(FedimixError::InsufficientFunds { .. })),
        "{reply:?}"
    );
    wait_until("the first payment", || async {
        harness.first.paid() == [INVOICE]
    })
    .await;
    assert_eq!(rows(&*backend).await.len(), 1, "the second left no row");
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn funds_spent_while_submitting_end_in_a_failed_row() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let events = backend.subscribe();
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    harness
        .first
        .fail_next_pay(FedimixError::InsufficientFunds {
            available: sat(1),
            needed: sat(210),
        });
    let id = backend.confirm_send(preview.id).await.unwrap();
    let (states, history_id) = run_of(&events, &id).await;
    assert!(
        matches!(
            states.last(),
            Some(OperationState::Failed {
                error: FedimixError::InsufficientFunds { .. }
            })
        ),
        "{states:?}"
    );
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn refund_restores_and_labels() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let events = backend.subscribe();
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let op = operation_of(&harness.first, &id).await;
    wait_balance(&*backend, sat(790)).await;
    harness.first.set_balance(sat(1_000));
    harness.first.push_update(&op, WatchUpdate::Refunded);

    let (states, history_id) = run_of(&events, &id).await;
    assert_eq!(states.last(), Some(&failed_with(CAME_BACK)));
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Refunded).await;
    wait_balance(&*backend, sat(1_000)).await;
    assert_eq!(
        notices(&drain(&events).await),
        0,
        "a refund confirms nothing"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn send_failure_keeps_history_row_failed() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let events = backend.subscribe();
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let op = operation_of(&harness.first, &id).await;
    let reason = "The payment failed.";
    harness.first.push_update(
        &op,
        WatchUpdate::Failed {
            reason: reason.into(),
        },
    );
    let (states, history_id) = run_of(&events, &id).await;
    assert_eq!(states.last(), Some(&failed_with(reason)));
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_terms_after_the_row_end_it_failed_for_a_new_preview() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let events = backend.subscribe();
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    harness.first.fail_next_pay(FedimixError::PreviewExpired);
    let id = backend.confirm_send(preview.id).await.unwrap();
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(
        states.last(),
        Some(&OperationState::Failed {
            error: FedimixError::PreviewExpired
        })
    );
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    assert_eq!(harness.first.paid(), Vec::<String>::new());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn max_leaves_nothing_after_fee() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = backend
        .prepare_send(to(ADDRESS, SendAmount::Max))
        .await
        .unwrap();
    assert_eq!(preview.amount, sat(700));
    assert_eq!(preview.fee, sat(300));
    assert_eq!(preview.change, ChangeDestination::None);
    let id = backend.confirm_send(preview.id).await.unwrap();
    let op = operation_of(&harness.first, &id).await;
    wait_balance(&*backend, Amount::ZERO).await;
    assert_eq!(harness.first.withdrawn()[0].amount, sat(700));
    let row = row_of(&rows(&*backend).await, &id);
    assert_eq!(row.kind, HistoryKind::Sent { change: None });
    assert_eq!(row.amount, sat(700));
    harness.first.push_update(&op, settled(700));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn max_of_less_than_the_fee_is_refused() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(100)).await;
    let reply = backend.prepare_send(to(ADDRESS, SendAmount::Max)).await;
    assert!(
        matches!(reply, Err(FedimixError::InsufficientFunds { .. })),
        "{reply:?}"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_above_the_balance_is_insufficient() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    assert_eq!(
        backend
            .prepare_send(to(ADDRESS, SendAmount::Exact(sat(701))))
            .await,
        Err(FedimixError::InsufficientFunds {
            available: sat(1_000),
            needed: sat(1_001),
        })
    );
    let whole = backend
        .prepare_send(to(ADDRESS, SendAmount::Exact(sat(700))))
        .await
        .unwrap();
    assert_eq!(whole.amount, sat(700), "a send may use the whole balance");
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn wrong_network_address_refused() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let reply = backend
        .prepare_send(to(MAINNET_ADDRESS, SendAmount::Exact(sat(1))))
        .await;
    assert_eq!(
        reply,
        Err(FedimixError::InvalidDestination(
            "This address belongs to a different bitcoin network".into()
        ))
    );
    let reply = backend
        .prepare_send(to("bcrt1qbadchecksum", SendAmount::Exact(sat(1))))
        .await;
    assert!(
        matches!(reply, Err(FedimixError::InvalidDestination(_))),
        "{reply:?}"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_invoice_refused() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let expired = FedimixError::InvalidDestination("This invoice has expired.".into());
    harness.first.fail_next_quote(expired.clone());
    assert_eq!(backend.prepare_send(pay_request()).await, Err(expired));
    assert_eq!(harness.first.pay_calls(), 0);
    backend.shutdown().await.unwrap();
}

/// LNv2 has no amount of its own to pay an amountless invoice with, so an amount
/// entered by hand does not help (see `decisions.md`, Task 5).
#[tokio::test]
async fn amountless_invoice_needs_exact() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    for amount in [SendAmount::FromInvoice, SendAmount::Exact(sat(100))] {
        let reply = backend.prepare_send(to("lnbcrt1abc", amount)).await;
        assert!(
            matches!(reply, Err(FedimixError::InvalidAmount(_))),
            "{reply:?}"
        );
    }
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_invoice_sets_its_own_amount() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    for amount in [SendAmount::Exact(sat(100)), SendAmount::Max] {
        let reply = backend.prepare_send(to(INVOICE, amount)).await;
        assert!(
            matches!(reply, Err(FedimixError::InvalidAmount(_))),
            "{reply:?}"
        );
    }
    let same = backend
        .prepare_send(to(INVOICE, SendAmount::Exact(sat(200))))
        .await;
    assert_eq!(same.unwrap().amount, sat(200));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_address_needs_an_amount() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let reply = backend
        .prepare_send(to(ADDRESS, SendAmount::FromInvoice))
        .await;
    assert!(
        matches!(reply, Err(FedimixError::InvalidAmount(_))),
        "{reply:?}"
    );
    let uri = format!("bitcoin:{ADDRESS}?amount=0.00000500");
    let carried = backend
        .prepare_send(to(&uri, SendAmount::FromInvoice))
        .await;
    assert_eq!(carried.unwrap().amount, sat(500));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn offline_prepare_is_offline() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    harness.first.set_online(false);
    wait_online(&*backend, false).await;
    assert_eq!(
        backend.prepare_send(pay_request()).await,
        Err(FedimixError::Offline)
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn sends_need_spending_and_a_federation() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    let reply = backend.prepare_send(pay_request()).await;
    assert!(matches!(reply, Err(FedimixError::NotSetUp(_))), "{reply:?}");
    let saving = SendRequest {
        wallet: WalletKind::Saving,
        ..pay_request()
    };
    assert_eq!(
        backend.prepare_send(saving).await,
        Err(FedimixError::NotSetUp(
            "Saving arrives in a later version.".into()
        ))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failure_before_the_row_is_an_error_and_leaves_none() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    harness.first.set_online(false);
    wait_online(&*backend, false).await;
    assert_eq!(
        backend.confirm_send(preview.id.clone()).await,
        Err(FedimixError::Offline)
    );
    assert!(rows(&*backend).await.is_empty());
    assert_eq!(harness.first.pay_calls(), 0);
    harness.first.set_online(true);
    wait_online(&*backend, true).await;
    assert_eq!(
        backend.confirm_send(preview.id.clone()).await,
        Err(FedimixError::UnknownItem(preview.id)),
        "the preview is used up: the user previews again"
    );
    backend.shutdown().await.unwrap();
}

async fn on_chain_preview(backend: &dyn Backend) -> SendPreview {
    backend
        .prepare_send(to(ADDRESS, SendAmount::Exact(sat(500))))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_higher_fee_at_confirm_expires_the_preview() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = on_chain_preview(&*backend).await;
    harness.first.set_withdraw_fee(sat(301));
    assert_eq!(
        backend.confirm_send(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
    assert!(harness.first.withdrawn().is_empty());
    assert!(rows(&*backend).await.is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_lower_or_equal_fee_at_confirm_is_the_one_used() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(2_000)).await;
    let preview = on_chain_preview(&*backend).await;
    harness.first.set_withdraw_fee(sat(250));
    backend.confirm_send(preview.id).await.unwrap();
    wait_until("the withdrawal", || async {
        harness.first.withdrawn().len() == 1
    })
    .await;
    assert_eq!(harness.first.withdrawn()[0].fee, sat(250));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_quote_at_confirm_leaves_no_row() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = on_chain_preview(&*backend).await;
    harness.first.fail_next_quote(FedimixError::Offline);
    assert_eq!(
        backend.confirm_send(preview.id).await,
        Err(FedimixError::Offline)
    );
    assert!(rows(&*backend).await.is_empty());
    assert!(harness.first.withdrawn().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_resumes_running_send() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let op = operation_of(&harness.first, &id).await;
    wait_until("the operation id on disk", || async {
        raw_store(&harness)
            .query_row(
                "SELECT operation FROM watches WHERE id = ?1",
                [&id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
            == Some(op.clone())
    })
    .await;

    let backend = restarted(&harness, backend).await;
    assert_eq!(
        backend.operations().await.unwrap(),
        vec![RunningOperation {
            id: id.clone(),
            kind: OperationKind::Send,
            state: OperationState::Broadcasting,
        }]
    );
    let events = backend.subscribe();
    harness.first.push_update(&op, settled(200));
    let (states, history_id) = run_of(&events, &id).await;
    assert_eq!(states, [OperationState::Done]);
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    assert_eq!(harness.first.paid(), [INVOICE]);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn crash_before_pay_resolves_failed() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    halt_sends(&backend, SendHalt::BeforeSubmit);
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();

    let backend = restarted(&harness, backend).await;
    let events = backend.subscribe();
    let (states, history_id) = run_of(&events, &id).await;
    assert_eq!(states, [failed_with(NEVER_SENT)]);
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    assert_eq!(harness.first.pay_calls(), 0, "nothing pays by itself");
    assert!(backend.operations().await.unwrap().is_empty());
    assert_eq!(watches_left(&harness), 0);

    let backend = restarted(&harness, backend).await;
    let events = backend.subscribe();
    wait_online(&*backend, true).await;
    let later = drain(&events).await;
    assert!(
        !later
            .iter()
            .any(|event| matches!(event, Event::Operation { .. })),
        "settled once and for all: {later:?}"
    );
    assert_eq!(harness.first.pay_calls(), 0);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn killed_after_submit_resumes() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    halt_sends(&backend, SendHalt::AfterSubmit);
    let events = backend.subscribe();
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let op = operation_of(&harness.first, &id).await;
    assert_eq!(notices(&drain(&events).await), 0);

    let backend = restarted(&harness, backend).await;
    let events = backend.subscribe();
    harness.first.push_update(&op, settled(200));
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(states, [OperationState::Done]);
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    assert_eq!(harness.first.paid(), [INVOICE], "exactly one debit");
    wait_balance(&*backend, sat(790)).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_lookup_leaves_the_row_for_the_next_connect() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    halt_sends(&backend, SendHalt::BeforeSubmit);
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.first.fail_next_find(FedimixError::Offline);
    let backend = harness.open().await.unwrap();
    wait_online(&*backend, true).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        row_of(&rows(&*backend).await, &id).status,
        HistoryStatus::Pending
    );
    assert_eq!(backend.operations().await.unwrap().len(), 1);

    harness.first.set_online(false);
    wait_online(&*backend, false).await;
    harness.first.set_online(true);
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unfinished_intent_waits_for_the_federation() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    halt_sends(&backend, SendHalt::BeforeSubmit);
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.first.set_online(false);
    let backend = harness.open().await.unwrap();
    wait_until("the client to open", || async {
        harness.port.connects() >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        row_of(&rows(&*backend).await, &id).status,
        HistoryStatus::Pending
    );
    harness.first.set_online(true);
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    backend.shutdown().await.unwrap();
}

/// `find_operation` knows nothing while `pay_invoice` runs, and a reconnect must
/// not take that for "never sent".
#[tokio::test]
async fn a_send_still_being_submitted_is_left_alone() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let events = backend.subscribe();
    harness.first.set_pay_held(true);
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_until("the payment to start", || async {
        harness.first.pay_calls() == 1
    })
    .await;

    harness.first.set_online(false);
    wait_online(&*backend, false).await;
    harness.first.set_online(true);
    wait_online(&*backend, true).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        row_of(&rows(&*backend).await, &id).status,
        HistoryStatus::Pending
    );

    harness.first.set_pay_held(false);
    let op = operation_of(&harness.first, &id).await;
    harness.first.push_update(&op, settled(200));
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(states.last(), Some(&OperationState::Done));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_a_payment_in_flight() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    harness.first.set_pay_held(true);
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_until("the payment to start", || async {
        harness.first.pay_calls() == 1
    })
    .await;

    let closing = Arc::clone(&backend);
    let closing = tokio::spawn(async move { closing.shutdown().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!closing.is_finished());
    harness.first.set_pay_held(false);
    closing.await.unwrap().unwrap();
    drop(backend);

    let op = harness.first.find_operation(&id).await.unwrap();
    let stored: Option<String> = raw_store(&harness)
        .query_row(
            "SELECT operation FROM watches WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, op, "the id was saved before the folder was let go");
}

#[tokio::test]
async fn a_send_notifies_once_also_when_replayed() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let events = backend.subscribe();
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let op = operation_of(&harness.first, &id).await;
    harness.first.push_update(&op, settled(200));
    within("the notice", async {
        loop {
            if let Some(Event::Notification(notice)) = events.next().await {
                assert_eq!(notice, Notice::SendConfirmed { amount: sat(200) });
                return;
            }
        }
    })
    .await;
    wait_until("the watch to finish", || async {
        watches_left(&harness) == 0
    })
    .await;

    raw_store(&harness)
        .execute("UPDATE watches SET finished = 0 WHERE id = ?1", [&id])
        .unwrap();
    let backend = restarted(&harness, backend).await;
    let events = backend.subscribe();
    let (states, history_id) = run_of(&events, &id).await;
    assert_eq!(states, [OperationState::Done]);
    assert_eq!(history_id, Some(id));
    assert_eq!(notices(&drain(&events).await), 0);
    assert!(backend.operations().await.unwrap().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_unknown_errors() {
    let harness = Harness::new();
    let backend = harness.open().await.unwrap();
    assert_eq!(
        backend.cancel_operation("nothing".into()).await,
        Err(FedimixError::UnknownItem("nothing".into()))
    );
    assert_eq!(
        backend.confirm_send("nothing".into()).await,
        Err(FedimixError::UnknownItem("nothing".into()))
    );
    assert!(backend.operations().await.unwrap().is_empty());
    backend.discard_preview("nothing".into()).await.unwrap();
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_receive_is_not_an_operation_to_cancel() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let info = backend.receive(invoice_request(5)).await.unwrap();
    let id = watch_id(&info.payload).to_string();
    assert_eq!(
        backend.cancel_operation(id.clone()).await,
        Err(FedimixError::UnknownItem(id))
    );
    assert!(backend.operations().await.unwrap().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_higher_lightning_fee_at_confirm_expires_the_preview() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    harness.first.set_lightning_fee(sat(11));
    assert_eq!(
        backend.confirm_send(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
    assert_eq!(harness.first.pay_calls(), 0);
    assert!(rows(&*backend).await.is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_lower_lightning_fee_at_confirm_is_the_one_paid() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    harness.first.set_lightning_fee(sat(4));
    backend.confirm_send(preview.id).await.unwrap();
    wait_balance(&*backend, sat(796)).await;
    backend.shutdown().await.unwrap();
}

const LOST_RECORD: &str = "Fedimix lost its record of this payment and cannot tell whether it was sent. Check your balance before paying again.";

/// The sentence is written before `recover`, so it holds with recovery cut off.
#[tokio::test]
async fn a_lost_client_database_does_not_claim_an_intent_was_never_sent() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    halt_sends(&backend, SendHalt::BeforeSubmit);
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);

    harness
        .port
        .set_open_error(Some(FedimixError::UnknownItem("one".into())));
    harness.port.set_recover_held(true);
    let backend = harness.open().await.unwrap();
    let events = backend.subscribe();
    let (states, history_id) = run_of(&events, &id).await;
    assert_eq!(states, [failed_with(LOST_RECORD)]);
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    assert_eq!(harness.port.recovers(), 1);
    assert_eq!(watches_left(&harness), 0);
    assert_eq!(harness.first.pay_calls(), 0);
    harness.port.set_recover_held(false);
    backend.shutdown().await.unwrap();
}

/// On a connection that stays up, no online change will come to try again.
#[tokio::test(start_paused = true)]
async fn a_failed_lookup_is_tried_again_on_a_steady_connection() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    halt_sends(&backend, SendHalt::BeforeSubmit);
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);

    harness.first.fail_next_find(FedimixError::Offline);
    let backend = harness.open().await.unwrap();
    wait_online(&*backend, true).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        row_of(&rows(&*backend).await, &id).status,
        HistoryStatus::Pending
    );
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert_eq!(
        row_of(&rows(&*backend).await, &id).status,
        HistoryStatus::Failed
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_settled_send_keeps_the_amount_of_its_row() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(1_000)).await;
    let events = backend.subscribe();
    let preview = backend.prepare_send(pay_request()).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let op = operation_of(&harness.first, &id).await;
    harness.first.push_update(&op, settled(0));
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(states.last(), Some(&OperationState::Done));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    assert_eq!(row_of(&rows(&*backend).await, &id).amount, sat(200));
    backend.shutdown().await.unwrap();
}
