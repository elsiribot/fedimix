//! `RealBackend` over the scripted federation.
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bdk_wallet::bitcoin::hashes::Hash as _;
use bdk_wallet::bitcoin::{
    self, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use bdk_wallet::chain::spk_client::{FullScanResponse, SyncResponse};
use bdk_wallet::chain::{BlockId, CheckPoint, ConfirmationBlockTime, TxUpdate};
use fedimix_core::testing::{
    BroadcastGoes, ChainClient, ChainFactory, DepositEvent, DepositSource, FakeChain, FakeClient,
    FakeDevices, FakePort, FederationClient, SendHalt, SessionDeposit, Signer, SoftSigner,
    WatchUpdate, halt_sends, item_estimate, real_backend, regtest_address, sync_saving_now,
};
use fedimix_core::{
    Amount, Backend, ChainBackend, ChangeDestination, CoreConfig, DeviceKind, Event, EventStream,
    Features, FedimixError, HistoryItem, HistoryKind, HistoryStatus, Network, NoteBucket, Notice,
    OperationKind, OperationState, PrivacyTag, ReceiveMethod, ReceiveRequest, RefillPreview,
    Result, RunningOperation, SendAmount, SendPreview, SendRequest, Settings, WalletKind,
};
use tempfile::TempDir;
use tokio::sync::watch;

const INVITE: &str = "fed1one";
const SECOND_INVITE: &str = "fed1two";
const RESTORING: &str = "Fedimix is still restoring your wallet. Try again when it has finished.";
const SET_UP_FIRST: &str = "Set up Saving first.";
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
async fn a_failed_open_tells_a_subscriber_its_problem() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);

    // The first open hangs until a subscriber is attached, as a front end
    // takes its snapshot before the supervisor's first answer.
    harness.port.set_held(true);
    harness
        .port
        .set_open_error(Some(FedimixError::Internal(WRONG_PHRASE.into())));
    let backend = harness.open().await.unwrap();
    let events = backend.subscribe();
    harness.port.set_held(false);
    wait_event(&events, &Event::FederationChanged).await;
    let federation = backend.spending_state().await.unwrap().federation.unwrap();
    assert_eq!(federation.problem.as_deref(), Some(WRONG_PHRASE));

    // A different problem is news too.
    harness
        .port
        .set_open_error(Some(FedimixError::Internal("Another problem.".into())));
    wait_event(&events, &Event::FederationChanged).await;
    let federation = backend.spending_state().await.unwrap().federation.unwrap();
    assert_eq!(federation.problem.as_deref(), Some("Another problem."));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn spending_state_splits_at_target() {
    let harness = Harness::new();
    harness.first.set_balance(Amount::from_sat(7_000_000));
    let backend = harness.joined().await;
    wait_queue(&*backend, &[Amount::from_sat(2_000_000)]).await;
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.target, Amount::from_sat(5_000_000));
    assert_eq!(state.for_spending, Amount::from_sat(5_000_000));
    assert_eq!(state.moving, Amount::from_sat(2_000_000));
    assert!(!state.saving_ready);

    // Only a send or a higher target shrinks the queue: money that leaves
    // otherwise comes from the part for spending.
    harness.first.set_balance(Amount::from_sat(3_000_000));
    wait_balance(&*backend, Amount::from_sat(3_000_000)).await;
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.for_spending, Amount::from_sat(1_000_000));
    assert_eq!(state.moving, Amount::from_sat(2_000_000));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn queue_waits_for_saving() {
    let harness = Harness::new();
    harness.first.set_balance(Amount::from_sat(7_000_000));
    let backend = harness.joined().await;
    wait_queue(&*backend, &[Amount::from_sat(2_000_000)]).await;
    let queue = backend.queue_state().await.unwrap();
    assert!(queue.waiting_for_saving);
    assert_eq!(queue.items[0].status, fedimix_core::QueueItemStatus::Queued);
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
    assert_eq!(
        backend.setup_saving("device".into()).await,
        Err(FedimixError::DeviceNotConnected("hardware wallet".into()))
    );
    assert_eq!(
        backend.set_coin_frozen("o".into(), true).await,
        Err(FedimixError::NotSetUp(SET_UP_FIRST.into()))
    );
    assert_eq!(
        backend.prepare_refill().await.map(|_| ()),
        Err(FedimixError::NotSetUp(SET_UP_FIRST.into()))
    );
    assert_eq!(
        backend.confirm_refill("p".into()).await.map(|_| ()),
        Err(FedimixError::UnknownItem("p".into()))
    );
    assert_eq!(
        backend.prepare_exit("i".into()).await.map(|_| ()),
        Err(FedimixError::UnknownItem("i".into()))
    );
    assert_eq!(
        backend.exit_now("p".into()).await.map(|_| ()),
        Err(FedimixError::UnknownItem("p".into()))
    );
    // No device, but the software wallet is always there.
    let found = backend.detect_devices().await.unwrap().devices;
    let kinds: Vec<_> = found.iter().map(|d| d.kind).collect();
    assert_eq!(kinds, [DeviceKind::Software]);
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
        ..Settings::for_network(Network::Regtest)
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
        txid: None,
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
async fn only_an_address_without_a_watch_repeats() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let request = || ReceiveRequest {
        wallet: WalletKind::Spending,
        method: ReceiveMethod::OnChain,
    };
    assert!(!backend.receive(request()).await.unwrap().address_repeats);
    harness.first.set_deposit_watches(false);
    assert!(backend.receive(request()).await.unwrap().address_repeats);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_invoice_states_its_fee_when_the_client_knows_it() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let request = || ReceiveRequest {
        wallet: WalletKind::Spending,
        method: ReceiveMethod::Lightning {
            amount: Amount::from_sat(1_000),
            description: String::new(),
            expiry_secs: 600,
        },
    };
    harness.first.set_receive_fee(Some(Amount::from_sat(7)));
    let info = backend.receive(request()).await.unwrap();
    assert_eq!(info.fee, Some(Amount::from_sat(7)));
    assert!(!info.address_repeats);
    harness.first.set_receive_fee(None);
    assert_eq!(backend.receive(request()).await.unwrap().fee, None);
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
        Err(FedimixError::NotSetUp(SET_UP_FIRST.into()))
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
    assert_eq!(
        state.features,
        Features {
            saving: true,
            ..Features::NONE
        }
    );
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
    assert!(
        sentence.contains("or choose another data folder"),
        "{sentence}"
    );
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
async fn missing_seed_with_a_surviving_federations_folder_is_refused() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    // The real port makes `federations/<id>` on a join; the fake keeps no files.
    std::fs::create_dir_all(harness.dir.path().join("federations").join("one")).unwrap();
    lose_store(&harness);
    assert_refused_for_a_missing_seed(&harness).await;
}

#[tokio::test]
async fn an_unreadable_federations_folder_is_an_error_not_no_data() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    let federations = harness.dir.path().join("federations");
    std::fs::create_dir_all(federations.join("one")).unwrap();
    lose_store(&harness);
    std::fs::remove_file(harness.dir.path().join("seed")).unwrap();
    std::fs::set_permissions(&federations, std::fs::Permissions::from_mode(0o000)).unwrap();
    let reply = harness.open().await;
    std::fs::set_permissions(&federations, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Err(FedimixError::Internal(sentence)) = &reply else {
        panic!(
            "started beside a folder it could not read: {:?}",
            reply.map(|_| ())
        );
    };
    assert!(sentence.contains("federations"), "{sentence}");
    assert!(!harness.dir.path().join("seed").exists());
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
    run_of_kind(events, id, OperationKind::Send).await
}

/// As [`run_of`], for an operation of `wanted` kind.
async fn run_of_kind(
    events: &EventStream,
    id: &str,
    wanted: OperationKind,
) -> (Vec<OperationState>, Option<String>) {
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
            assert_eq!(kind, wanted);
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
        ..to(ADDRESS, SendAmount::Exact(sat(500)))
    };
    assert_eq!(
        backend.prepare_send(saving).await,
        Err(FedimixError::NotSetUp(SET_UP_FIRST.into()))
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

/// Confirms an on-chain preview and returns its intent and operation.
async fn withdrawn(harness: &Harness, backend: &dyn Backend) -> (String, String) {
    let preview = on_chain_preview(backend).await;
    let intent = backend.confirm_send(preview.id).await.unwrap();
    let first = &harness.first;
    within("the operation", async {
        loop {
            if let Some(operation) = first.find_operation(&intent).await.unwrap() {
                return (intent.clone(), operation);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
}

#[tokio::test]
async fn a_settled_withdrawal_keeps_its_fee_and_txid() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(2_000)).await;
    let (intent, operation) = withdrawn(&harness, &*backend).await;
    let row = row_of(&rows(&*backend).await, &intent);
    assert_eq!(row.fee, Some(sat(300)), "the fee shown, until a better one");
    assert_eq!(row.txid, None);

    harness.first.push_update(
        &operation,
        WatchUpdate::Settled {
            amount: sat(500),
            fee: sat(290),
            txid: Some("txid".into()),
        },
    );
    wait_status(&*backend, &intent, HistoryStatus::Complete).await;
    let row = row_of(&rows(&*backend).await, &intent);
    assert_eq!(row.fee, Some(sat(290)));
    assert_eq!(row.txid.as_deref(), Some("txid"));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_send_with_unreadable_details_keeps_the_fee_it_was_shown() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(2_000)).await;
    let (intent, operation) = withdrawn(&harness, &*backend).await;
    harness.first.push_update(
        &operation,
        WatchUpdate::Settled {
            amount: Amount::ZERO,
            fee: Amount::ZERO,
            txid: None,
        },
    );
    wait_status(&*backend, &intent, HistoryStatus::Complete).await;
    assert_eq!(row_of(&rows(&*backend).await, &intent).fee, Some(sat(300)));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_deposit_row_keeps_its_fee_and_txid() {
    let harness = Harness::new();
    harness.first.set_deposit_watches(false);
    let backend = harness.joined().await;
    harness.first.push_deposit(deposit_event(1, "first", true));
    wait_status(&*backend, "first", HistoryStatus::Complete).await;
    let row = row_of(&rows(&*backend).await, "first");
    assert_eq!(row.fee, Some(sat(100)));
    assert_eq!(row.txid.as_deref(), Some("txid"));
    backend.shutdown().await.unwrap();
}

fn joined_session(harness: &Harness) -> Option<i64> {
    raw_store(harness)
        .query_row("SELECT joined_session FROM federations", [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn a_join_records_the_session_count() {
    let harness = Harness::new();
    harness.first.set_session_count(Ok(41));
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    assert_eq!(joined_session(&harness), Some(41));
}

#[tokio::test]
async fn a_join_survives_an_unreadable_session_count() {
    let harness = Harness::new();
    harness.first.set_session_count(Err(FedimixError::Offline));
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    assert_eq!(joined_session(&harness), None);
}

#[tokio::test]
async fn a_settled_deposit_records_the_session_count_once() {
    let harness = Harness::new();
    harness.first.set_deposit_watches(false);
    harness.first.set_session_count(Ok(50));
    let backend = harness.joined().await;
    harness.first.push_deposit(deposit_event(1, "first", false));
    wait_status(&*backend, "first", HistoryStatus::Pending).await;
    assert_eq!(row_of(&rows(&*backend).await, "first").session, None);

    harness.first.push_deposit(deposit_event(2, "first", true));
    wait_status(&*backend, "first", HistoryStatus::Complete).await;
    assert_eq!(row_of(&rows(&*backend).await, "first").session, Some(50));

    // A replay of the same deposit keeps the first answer.
    harness.first.set_session_count(Ok(60));
    harness.first.push_deposit(deposit_event(3, "first", true));
    harness.first.push_deposit(deposit_event(4, "second", true));
    wait_status(&*backend, "second", HistoryStatus::Complete).await;
    let rows = rows(&*backend).await;
    assert_eq!(row_of(&rows, "first").session, Some(50));
    assert_eq!(row_of(&rows, "second").session, Some(60));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_deposit_watch_records_the_session_count() {
    let harness = Harness::new();
    harness.first.set_session_count(Ok(70));
    let backend = harness.joined().await;
    let info = backend
        .receive(ReceiveRequest {
            wallet: WalletKind::Spending,
            method: ReceiveMethod::OnChain,
        })
        .await
        .unwrap();
    let id = watch_id(&info.payload).to_string();
    harness.first.push_update(
        &id,
        WatchUpdate::Settled {
            amount: sat(40_000),
            fee: sat(100),
            txid: Some("txid".into()),
        },
    );
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    assert_eq!(row_of(&rows(&*backend).await, &id).session, Some(70));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_deposit_settles_with_an_unreadable_session_count() {
    let harness = Harness::new();
    harness.first.set_deposit_watches(false);
    harness.first.set_session_count(Err(FedimixError::Offline));
    let backend = harness.joined().await;
    harness.first.push_deposit(deposit_event(1, "first", true));
    wait_status(&*backend, "first", HistoryStatus::Complete).await;
    assert_eq!(row_of(&rows(&*backend).await, "first").session, None);
    backend.shutdown().await.unwrap();
}

/// Own Lightning receives are subtracted by session, so they keep it as
/// deposits do: the first answer, read when they settle.
#[tokio::test]
async fn a_lightning_receive_keeps_its_session() {
    let harness = Harness::new();
    harness.first.set_session_count(Ok(80));
    let backend = harness.joined().await;
    let info = backend.receive(invoice_request(5_000)).await.unwrap();
    let id = watch_id(&info.payload).to_string();
    assert_eq!(row_of(&rows(&*backend).await, &id).session, None);
    harness.first.push_update(&id, settled(4_990));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    assert_eq!(row_of(&rows(&*backend).await, &id).session, Some(80));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_preview_names_the_federation_it_spends_from() {
    let harness = Harness::new();
    let backend = funded(&harness, sat(2_000)).await;
    let preview = on_chain_preview(&*backend).await;
    assert_eq!(preview.federation.as_deref(), Some("one"));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_federation_keeps_saying_whether_it_has_lightning() {
    let harness = Harness::new();
    harness
        .port
        .add_federation_without_lightning("fed1dark", "dark", Network::Regtest);
    let backend = harness.joined().await;
    assert!(backend.federations().await.unwrap()[0].has_lightning);
    let joined = backend.join_federation("fed1dark".into()).await.unwrap();
    assert!(!joined.has_lightning);
    backend.shutdown().await.unwrap();
    drop(backend);

    let backend = harness.open().await.unwrap();
    let rows = backend.federations().await.unwrap();
    assert!(!rows.iter().find(|f| f.id == "dark").unwrap().has_lightning);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_snapshot_totals_every_federation() {
    let harness = Harness::new();
    harness.first.set_balance(sat(1_000));
    harness.second.set_balance(sat(500));
    let backend = harness.joined().await;
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    wait_until("both balances", || async {
        backend.spending_state().await.unwrap().total_balance == sat(1_500)
    })
    .await;
    assert_eq!(backend.spending_state().await.unwrap().balance, sat(1_000));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_set_proxy_is_the_problem_while_it_stands() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let mut settings = backend.settings().await.unwrap();
    settings.socks5_proxy = Some("127.0.0.1:9050".into());
    backend.set_settings(settings.clone()).await.unwrap();
    let federation = backend.federations().await.unwrap().remove(0);
    assert_eq!(
        federation.problem.as_deref(),
        Some("With a proxy set, Fedimix stays offline. Clear the proxy in Settings to connect.")
    );
    settings.socks5_proxy = None;
    backend.set_settings(settings).await.unwrap();
    assert_eq!(backend.federations().await.unwrap()[0].problem, None);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_preview_confirmed_after_a_switch_pays_from_its_own_federation() {
    let harness = Harness::new();
    harness.second.set_balance(sat(9_000));
    let backend = funded(&harness, sat(2_000)).await;
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    let preview = on_chain_preview(&*backend).await;
    backend.set_active_federation("two".into()).await.unwrap();
    wait_online(&*backend, true).await;
    let intent = backend.confirm_send(preview.id).await.unwrap();
    wait_until("the withdrawal", || async {
        harness.first.withdrawn().len() == 1
    })
    .await;
    assert!(harness.second.withdrawn().is_empty());
    assert!(
        rows(&*backend).await.is_empty(),
        "not a row of the active one"
    );
    backend.set_active_federation("one".into()).await.unwrap();
    row_of(&rows(&*backend).await, &intent);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_deposit_to_a_federation_not_active_stays_out_of_the_active_list() {
    let harness = Harness::new();
    harness.second.set_deposit_watches(false);
    let backend = harness.joined().await;
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    let events = backend.subscribe();
    harness
        .second
        .push_deposit(deposit_event(1, "elsewhere", true));
    wait_event(&events, &Event::FederationChanged).await;
    wait_until("the deposit's notice", || async {
        backend.set_active_federation("two".into()).await.unwrap();
        let found = rows(&*backend).await.iter().any(|r| r.id == "elsewhere");
        backend.set_active_federation("one".into()).await.unwrap();
        found
    })
    .await;
    assert!(rows(&*backend).await.is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn max_sends_from_the_active_balance() {
    let harness = Harness::new();
    harness.second.set_balance(sat(9_000));
    let backend = funded(&harness, sat(2_000)).await;
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    let max = to(ADDRESS, SendAmount::Max);
    let preview = backend.prepare_send(max.clone()).await.unwrap();
    assert_eq!(preview.amount, sat(1_700));

    backend.set_active_federation("two".into()).await.unwrap();
    wait_online(&*backend, true).await;
    wait_until("the second federation's preview", || async {
        backend
            .prepare_send(max.clone())
            .await
            .is_ok_and(|preview| preview.amount == sat(8_700))
    })
    .await;
    backend.shutdown().await.unwrap();
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

// ---- Saving ----

const CHAIN_URL: &str = "http://chain.test";
const DEVICE: &str = "ledger:one";
const SEED: [u8; 32] = [7; 32];
const SCANNING: &str = "Fedimix is still scanning Saving's history.";
const NO_SERVER: &str = "Enter a chain server in Settings.";
const JADE_PROXY: &str =
    "Jade needs Blockstream's PIN server, which cannot use your proxy. Clear the proxy to use it.";

type Real = Arc<fedimix_core::RealBackend>;

type ChainAsked = (ChainBackend, Option<String>);

struct SavingHarness {
    harness: Harness,
    chain: Arc<FakeChain>,
    devices: Arc<FakeDevices>,
    /// What the backend asked the chain factory for, in order.
    asked: Arc<Mutex<Vec<ChainAsked>>>,
}

fn chain_settings(url: &str) -> Settings {
    Settings {
        chain_backend: ChainBackend::Esplora { url: url.into() },
        ..Settings::for_network(Network::Regtest)
    }
}

fn soft_signer(network: bitcoin::Network) -> Arc<SoftSigner> {
    Arc::new(SoftSigner::from_seed(&SEED, network))
}

fn block(height: u32) -> BlockId {
    BlockId {
        height,
        hash: BlockHash::from_byte_array([height as u8 + 1; 32]),
    }
}

/// A chain from regtest's genesis up to `height`.
fn chain_to(height: u32) -> CheckPoint {
    let genesis = bitcoin::constants::genesis_block(bitcoin::Network::Regtest);
    let mut tip = CheckPoint::new(BlockId {
        height: 0,
        hash: genesis.block_hash(),
    });
    for h in 1..=height {
        tip = tip.insert(block(h));
    }
    tip
}

fn first_scan(height: u32) -> FullScanResponse<bdk_wallet::KeychainKind> {
    FullScanResponse {
        chain_update: Some(chain_to(height)),
        ..Default::default()
    }
}

/// A payment of `sat` to `address`; `n` makes it unique.
fn payment(address: &str, sat: u64, n: u32) -> Transaction {
    let address = bitcoin::Address::from_str(address)
        .unwrap()
        .assume_checked();
    Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: bitcoin::Txid::from_byte_array([n as u8 + 1; 32]),
                vout: n,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: bitcoin::Amount::from_sat(sat),
            script_pubkey: address.script_pubkey(),
        }],
    }
}

/// The sync that shows `tx` in a block at `height`.
fn confirmed(tx: Transaction, height: u32) -> SyncResponse {
    let txid = tx.compute_txid();
    let mut tx_update = TxUpdate::default();
    tx_update.txs = vec![Arc::new(tx)];
    tx_update.anchors = [(
        ConfirmationBlockTime {
            block_id: block(height),
            confirmation_time: 1_000 + u64::from(height),
        },
        txid,
    )]
    .into();
    SyncResponse {
        tx_update,
        chain_update: Some(chain_to(height)),
    }
}

/// The sync that shows `tx` in the mempool, seen at `at`.
fn in_mempool(tx: Transaction, at: u64, height: u32) -> SyncResponse {
    let txid = tx.compute_txid();
    let mut tx_update = TxUpdate::default();
    tx_update.txs = vec![Arc::new(tx)];
    tx_update.seen_ats = [(txid, at)].into();
    SyncResponse {
        tx_update,
        chain_update: Some(chain_to(height)),
    }
}

fn on_chain() -> ReceiveRequest {
    ReceiveRequest {
        wallet: WalletKind::Saving,
        method: ReceiveMethod::OnChain,
    }
}

impl SavingHarness {
    fn new() -> Self {
        let devices = Arc::new(FakeDevices::new());
        devices.add(
            DEVICE,
            "Test device",
            DeviceKind::Ledger,
            soft_signer(bitcoin::Network::Regtest),
        );
        Self {
            harness: Harness::new(),
            chain: Arc::new(FakeChain::new()),
            devices,
            asked: Arc::default(),
        }
    }

    async fn open(&self) -> Result<Real> {
        let config = CoreConfig {
            data_dir: self.harness.dir.path().to_str().unwrap().to_string(),
            network: Network::Regtest,
        };
        let chain = Arc::clone(&self.chain);
        let asked = Arc::clone(&self.asked);
        let factory: ChainFactory = Arc::new(move |backend, proxy| {
            asked
                .lock()
                .unwrap()
                .push((backend.clone(), proxy.map(str::to_string)));
            Ok(Arc::clone(&chain) as Arc<dyn ChainClient>)
        });
        fedimix_core::RealBackend::with_ports(
            config,
            self.harness.port.clone(),
            factory,
            self.devices.clone(),
        )
        .await
    }

    /// Open, with a chain server set, and not set up.
    async fn with_server(&self) -> Real {
        let backend = self.open().await.unwrap();
        backend
            .set_settings(chain_settings(CHAIN_URL))
            .await
            .unwrap();
        backend
    }

    /// Set up, with the first scan done at height 1.
    async fn ready(&self) -> Real {
        self.ready_with(DEVICE).await
    }

    /// As [`Self::ready`], set up with the device `id`.
    async fn ready_with(&self, id: &str) -> Real {
        let backend = self.with_server().await;
        self.chain.push_full_scan(first_scan(1));
        backend.setup_saving(id.into()).await.unwrap();
        wait_chain(&*backend, true).await;
        backend
    }

    /// Pays the next address of `backend` and waits to see the money.
    async fn fund(&self, backend: &Real, sat: u64, n: u32) -> String {
        let info = backend.receive(on_chain()).await.unwrap();
        let tx = payment(&info.payload, sat, n);
        let outpoint = format!("{}:0", tx.compute_txid());
        self.chain.push_sync(confirmed(tx, 2 + n));
        sync_saving_now(backend);
        wait_until("the funded coin", || async {
            let state = backend.saving_state().await.unwrap();
            state.coins.iter().any(|c| c.outpoint == outpoint)
        })
        .await;
        outpoint
    }

    fn last_asked(&self) -> Option<ChainAsked> {
        self.asked.lock().unwrap().last().cloned()
    }
}

async fn wait_chain(backend: &dyn Backend, online: bool) {
    wait_until("the chain's online state", || async {
        backend.saving_state().await.unwrap().chain_online == online
    })
    .await;
}

#[tokio::test]
async fn setup_stores_the_account_and_never_a_private_key() {
    let saving = SavingHarness::new();
    let backend = saving.with_server().await;
    let events = backend.subscribe();
    assert!(!backend.spending_state().await.unwrap().saving_ready);
    saving.chain.push_full_scan(first_scan(1));
    backend.setup_saving(DEVICE.into()).await.unwrap();
    wait_event(&events, &Event::SavingChanged).await;

    let state = backend.saving_state().await.unwrap();
    let device = state.device.unwrap();
    assert_eq!(device.name, "Test device");
    assert_eq!(device.kind, DeviceKind::Ledger);
    assert!(device.connected);
    assert!(backend.spending_state().await.unwrap().saving_ready);
    assert_eq!(saving.devices.connects(), 1);
    backend.shutdown().await.unwrap();

    let dir = saving.harness.dir.path();
    let mut account_seen = false;
    for name in ["fedimix.db", "fedimix.db-wal"] {
        let bytes = std::fs::read(dir.join(name)).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("prv"), "{name} holds a private key");
        account_seen |= text.contains("tpub");
    }
    assert!(account_seen, "the account's public key is stored");
}

#[tokio::test]
async fn setup_twice_is_refused() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    assert_eq!(
        backend.setup_saving(DEVICE.into()).await,
        Err(FedimixError::NotSetUp("Saving is already set up.".into()))
    );
    // The refusal needed no device.
    assert_eq!(saving.devices.connects(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_device_on_the_wrong_kind_of_network_gets_a_sentence() {
    let saving = SavingHarness::new();
    saving.devices.add(
        "ledger:main",
        "Main device",
        DeviceKind::Ledger,
        soft_signer(bitcoin::Network::Bitcoin),
    );
    let backend = saving.with_server().await;
    assert_eq!(
        backend.setup_saving("ledger:main".into()).await,
        Err(FedimixError::NotSetUp(
            "The Saving account is for the main network, but Fedimix is running on a test network."
                .into()
        ))
    );
    assert_eq!(backend.saving_state().await.unwrap().device, None);
    // Nothing was stored, so the right device can still be set up.
    saving.chain.push_full_scan(first_scan(1));
    backend.setup_saving(DEVICE.into()).await.unwrap();
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn reads_do_not_wait_for_the_chain() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    saving.chain.set_stalled(true);
    sync_saving_now(&backend);
    let quickly = Duration::from_secs(1);
    tokio::time::timeout(quickly, async {
        backend.saving_state().await.unwrap();
        backend.spending_state().await.unwrap();
        backend.history(WalletKind::Saving, None, 10).await.unwrap();
        backend.receive(on_chain()).await.unwrap();
    })
    .await
    .expect("reads answer while the chain hangs");
    // A hung server does not hold up closing either.
    within("shutdown", backend.shutdown()).await.unwrap();
}

#[tokio::test]
async fn chain_offline_shows_last_balance() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let events = backend.subscribe();
    let outpoint = saving.fund(&backend, 50_000, 0).await;
    wait_event(&events, &Event::SavingChanged).await;

    saving.chain.set_offline(true);
    sync_saving_now(&backend);
    wait_chain(&*backend, false).await;
    let state = backend.saving_state().await.unwrap();
    assert_eq!(state.balance, Amount::from_sat(50_000));
    assert_eq!(state.coins[0].outpoint, outpoint);
    assert_eq!(state.problem, None, "waiting cures being offline");
    assert_eq!(drain(&events).await, [Event::SavingChanged]);

    saving.chain.set_offline(false);
    sync_saving_now(&backend);
    wait_chain(&*backend, true).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_sync_that_learns_nothing_sends_no_event() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let events = backend.subscribe();
    let info = backend.receive(on_chain()).await.unwrap();
    let tx = payment(&info.payload, 20_000, 0);
    saving.chain.push_sync(in_mempool(tx.clone(), 1_000, 1));
    sync_saving_now(&backend);
    wait_until("the pending coin", || async {
        !backend.saving_state().await.unwrap().coins.is_empty()
    })
    .await;
    wait_event(&events, &Event::SavingChanged).await;
    let made = saving.chain.scans_made();
    // The same transaction, seen again later.
    saving.chain.push_sync(in_mempool(tx, 2_000, 1));
    sync_saving_now(&backend);
    wait_until("the second sync", || async {
        saving.chain.scans_made() > made
    })
    .await;
    assert_eq!(drain(&events).await, []);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_survives_restart() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let outpoint = saving.fund(&backend, 50_000, 0).await;
    let events = backend.subscribe();
    backend
        .set_coin_frozen(outpoint.clone(), true)
        .await
        .unwrap();
    wait_event(&events, &Event::SavingChanged).await;
    assert!(backend.saving_state().await.unwrap().coins[0].frozen);
    backend.shutdown().await.unwrap();
    drop(backend);

    let backend = saving.open().await.unwrap();
    let state = backend.saving_state().await.unwrap();
    assert!(state.coins[0].frozen);
    assert_eq!(
        state.balance,
        Amount::from_sat(50_000),
        "frozen stays yours"
    );
    backend.set_coin_frozen(outpoint, false).await.unwrap();
    assert!(!backend.saving_state().await.unwrap().coins[0].frozen);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn freezing_an_unknown_coin_is_refused() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    for outpoint in ["nonsense", &format!("{:064x}:0", 9)] {
        assert_eq!(
            backend.set_coin_frozen(outpoint.to_string(), true).await,
            Err(FedimixError::UnknownItem(outpoint.to_string()))
        );
    }
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn saving_receive_is_a_fresh_address_labelled_unmixed() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let first = backend.receive(on_chain()).await.unwrap();
    let second = backend.receive(on_chain()).await.unwrap();
    assert_ne!(first.payload, second.payload);
    assert!(first.payload.starts_with("bcrt1q"), "{first:?}");
    assert_eq!(first.uri, format!("bitcoin:{}", first.payload));
    assert!(!first.address_repeats);
    assert_eq!(first.expires_at, None);
    assert_eq!(first.min_deposit, None);

    saving
        .chain
        .push_sync(confirmed(payment(&second.payload, 30_000, 0), 2));
    sync_saving_now(&backend);
    wait_until("the coin", || async {
        !backend.saving_state().await.unwrap().coins.is_empty()
    })
    .await;
    let state = backend.saving_state().await.unwrap();
    assert_eq!(state.coins[0].tag, PrivacyTag::Unmixed);
    assert_eq!(state.coins[0].address, second.payload);
    assert!(state.coins[0].confirmed);
    assert_eq!(
        state.history[0].kind,
        HistoryKind::Received { unmixed: true }
    );
    backend.shutdown().await.unwrap();
}

/// An exit's coin names the federation it left, as the user knows it.
#[tokio::test]
async fn an_exit_coin_names_its_source_federation() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let federation = backend.federations().await.unwrap().remove(0);
    let address = backend.receive(on_chain()).await.unwrap().payload;
    let unknown = backend.receive(on_chain()).await.unwrap().payload;
    let raw = raw_store(&saving.harness);
    for (item, address, federation_id) in [
        ("a", &address, federation.id.as_str()),
        ("b", &unknown, "gone"),
    ] {
        raw.execute(
            "INSERT INTO exit_addresses
                (address, item_id, federation_id, denomination_msat, estimate, early)
             VALUES (?1, ?2, ?3, 100000000, 42, 0)",
            [address.as_str(), item, federation_id],
        )
        .unwrap();
    }
    for (n, address) in [&address, &unknown].into_iter().enumerate() {
        let n = n as u32;
        saving
            .chain
            .push_sync(confirmed(payment(address, 100_000, n), 2 + n));
        sync_saving_now(&backend);
        wait_until("the coin", || async {
            backend.saving_state().await.unwrap().coins.len() == n as usize + 1
        })
        .await;
    }
    let state = backend.saving_state().await.unwrap();
    let coin = |address: &str| {
        state
            .coins
            .iter()
            .find(|c| c.address == address)
            .unwrap()
            .clone()
    };
    assert_eq!(coin(&address).tag, PrivacyTag::Anonymity { estimate: 42 });
    assert_eq!(
        coin(&address).source_federation.as_deref(),
        Some(federation.name.as_str())
    );
    assert_eq!(coin(&unknown).source_federation.as_deref(), Some("gone"));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn saving_cannot_receive_over_lightning() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let request = ReceiveRequest {
        wallet: WalletKind::Saving,
        method: ReceiveMethod::Lightning {
            amount: Amount::from_sat(1_000),
            description: String::new(),
            expiry_secs: 60,
        },
    };
    assert!(matches!(
        backend.receive(request).await,
        Err(FedimixError::NotSetUp(_))
    ));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn receive_waits_for_the_first_scan() {
    let saving = SavingHarness::new();
    let backend = saving.with_server().await;
    saving.chain.set_stalled(true);
    saving.chain.push_full_scan(first_scan(1));
    backend.setup_saving(DEVICE.into()).await.unwrap();
    // An expected wait, not an error of Fedimix's.
    assert_eq!(
        backend.receive(on_chain()).await,
        Err(FedimixError::NotSetUp(SCANNING.into()))
    );
    saving.chain.set_stalled(false);
    wait_chain(&*backend, true).await;
    backend.receive(on_chain()).await.unwrap();
    backend.shutdown().await.unwrap();
}

/// I1: the state says a first scan is running, so a front end can say so and
/// not show a balance as known; it ends with the scan.
#[tokio::test]
async fn saving_state_says_while_the_first_scan_runs() {
    let saving = SavingHarness::new();
    let backend = saving.with_server().await;
    assert!(
        !backend.saving_state().await.unwrap().scanning,
        "not set up"
    );
    saving.chain.set_stalled(true);
    saving.chain.push_full_scan(first_scan(1));
    backend.setup_saving(DEVICE.into()).await.unwrap();
    let state = backend.saving_state().await.unwrap();
    assert!(state.scanning && state.wallet_open && !state.chain_online);
    assert_eq!(state.problem, None);
    saving.chain.set_stalled(false);
    wait_chain(&*backend, true).await;
    assert!(!backend.saving_state().await.unwrap().scanning);
    backend.shutdown().await.unwrap();
}

/// I2: what cannot be sent counts coins received and not yet confirmed, as
/// the balance leaves them out, and the review's note says the same.
#[tokio::test]
async fn held_back_counts_unconfirmed_coins_received() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    assert_eq!(
        backend.saving_state().await.unwrap().held_back,
        Amount::ZERO
    );
    let info = backend.receive(on_chain()).await.unwrap();
    let tx = payment(&info.payload, 40_000, 7);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    saving.chain.push_sync(in_mempool(tx, now, 2));
    wait_until("the unconfirmed coin held back", || async {
        sync_saving_now(&backend);
        backend.saving_state().await.unwrap().held_back == sat(40_000)
    })
    .await;
    let preview = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    assert!(
        preview
            .notes
            .contains(&fedimix_core::held_back_sentence(sat(40_000))),
        "{:?}",
        preview.notes
    );
    backend.shutdown().await.unwrap();
}

/// Nothing is scanning, so "still scanning" would be untrue.
#[tokio::test]
async fn receive_says_why_no_scan_can_finish() {
    let saving = SavingHarness::new();
    let backend = saving.open().await.unwrap();
    backend.setup_saving(DEVICE.into()).await.unwrap();
    wait_until("the problem", || async {
        backend.saving_state().await.unwrap().problem.is_some()
    })
    .await;
    assert_eq!(
        backend.receive(on_chain()).await,
        Err(FedimixError::NotSetUp(NO_SERVER.into()))
    );
    backend.shutdown().await.unwrap();
}

/// A device that can stop in its first read, or fail in its second.
struct ScriptedSigner {
    inner: SoftSigner,
    held: watch::Sender<bool>,
    xpub_fails: AtomicBool,
}

impl ScriptedSigner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: SoftSigner::from_seed(&SEED, bitcoin::Network::Regtest),
            held: watch::Sender::new(false),
            xpub_fails: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl Signer for ScriptedSigner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Software
    }

    async fn fingerprint(&self) -> Result<String> {
        let _ = self.held.subscribe().wait_for(|held| !held).await;
        self.inner.fingerprint().await
    }

    async fn xpub(&self, path: &str) -> Result<String> {
        if self.xpub_fails.swap(false, Ordering::SeqCst) {
            return Err(FedimixError::DeviceNotConnected("Test device".into()));
        }
        self.inner.xpub(path).await
    }

    async fn sign(&self, psbt: &mut bitcoin::psbt::Psbt) -> Result<()> {
        self.inner.sign(psbt).await
    }
}

const SCRIPTED: &str = "ledger:scripted";

fn with_scripted(saving: &SavingHarness) -> Arc<ScriptedSigner> {
    let signer = ScriptedSigner::new();
    saving
        .devices
        .add(SCRIPTED, "Scripted", DeviceKind::Ledger, signer.clone());
    signer
}

#[tokio::test]
async fn a_device_failing_between_its_keys_can_be_retried() {
    let saving = SavingHarness::new();
    let signer = with_scripted(&saving);
    signer.xpub_fails.store(true, Ordering::SeqCst);
    let backend = saving.with_server().await;
    assert_eq!(
        backend.setup_saving(SCRIPTED.into()).await,
        Err(FedimixError::DeviceNotConnected("Test device".into()))
    );
    assert_eq!(backend.saving_state().await.unwrap().device, None);
    assert!(!backend.spending_state().await.unwrap().saving_ready);
    saving.chain.push_full_scan(first_scan(1));
    backend.setup_saving(SCRIPTED.into()).await.unwrap();
    wait_chain(&*backend, true).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_proxy_set_during_setup_ends_it() {
    let saving = SavingHarness::new();
    let signer = with_scripted(&saving);
    signer.held.send_replace(true);
    let backend = saving.with_server().await;
    let setup = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { backend.setup_saving(SCRIPTED.into()).await })
    };
    wait_until("the device to open", || async {
        saving.devices.connects() == 1
    })
    .await;
    backend
        .set_settings(Settings {
            socks5_proxy: Some("127.0.0.1:9050".into()),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    assert!(matches!(
        within("the setup to end", setup).await.unwrap(),
        Err(FedimixError::NotSetUp(_))
    ));
    assert_eq!(backend.saving_state().await.unwrap().device, None);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_ends_a_setup_waiting_at_the_device() {
    let saving = SavingHarness::new();
    let signer = with_scripted(&saving);
    signer.held.send_replace(true);
    let backend = saving.with_server().await;
    let setup = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { backend.setup_saving(SCRIPTED.into()).await })
    };
    wait_until("the device to open", || async {
        saving.devices.connects() == 1
    })
    .await;
    within("shutdown", backend.shutdown()).await.unwrap();
    let ended = within("the setup to end", setup).await.unwrap();
    assert!(ended.is_err(), "{ended:?}");
    // It wrote nothing: the folder opens not set up.
    drop(backend);
    let backend = saving.open().await.unwrap();
    assert_eq!(backend.saving_state().await.unwrap().device, None);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_panic_in_a_sync_is_a_failed_sync() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    saving.chain.set_panics(true);
    sync_saving_now(&backend);
    wait_chain(&*backend, false).await;
    let state = backend.saving_state().await.unwrap();
    assert!(state.problem.is_some(), "{state:?}");
    // The task lives on, and the next sync works.
    saving.chain.set_panics(false);
    sync_saving_now(&backend);
    wait_chain(&*backend, true).await;
    assert_eq!(backend.saving_state().await.unwrap().problem, None);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_sync_finishing_after_a_settings_change_counts_once() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let info = backend.receive(on_chain()).await.unwrap();
    saving.chain.set_stalled(true);
    saving
        .chain
        .push_sync(confirmed(payment(&info.payload, 25_000, 0), 2));
    sync_saving_now(&backend);
    backend
        .set_settings(chain_settings("http://other.test"))
        .await
        .unwrap();
    let other = ChainBackend::Esplora {
        url: "http://other.test".into(),
    };
    wait_until("the new server", || async {
        saving.last_asked() == Some((other.clone(), None))
    })
    .await;
    saving.chain.set_stalled(false);
    wait_until("the coin", || async {
        backend.saving_state().await.unwrap().balance == Amount::from_sat(25_000)
    })
    .await;
    let state = backend.saving_state().await.unwrap();
    assert_eq!(state.coins.len(), 1);
    assert_eq!(state.history.len(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn proxy_applies_to_the_chain() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let server = ChainBackend::Esplora {
        url: CHAIN_URL.into(),
    };
    assert_eq!(saving.last_asked(), Some((server.clone(), None)));
    let proxy = "127.0.0.1:9050".to_string();
    backend
        .set_settings(Settings {
            socks5_proxy: Some(proxy.clone()),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    wait_until("the client through the proxy", || async {
        saving.last_asked() == Some((server.clone(), Some(proxy.clone())))
    })
    .await;
    wait_chain(&*backend, true).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    // And from the first sync after a restart.
    saving.asked.lock().unwrap().clear();
    let backend = saving.open().await.unwrap();
    wait_chain(&*backend, true).await;
    assert_eq!(saving.last_asked(), Some((server, Some(proxy))));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_new_chain_server_is_used_without_a_restart() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    // The old server hangs: the change must not wait for it.
    saving.chain.set_stalled(true);
    sync_saving_now(&backend);
    backend
        .set_settings(chain_settings("http://other.test"))
        .await
        .unwrap();
    let other = ChainBackend::Esplora {
        url: "http://other.test".into(),
    };
    wait_until("the new server", || async {
        saving.last_asked() == Some((other.clone(), None))
    })
    .await;
    saving.chain.set_stalled(false);
    wait_chain(&*backend, true).await;
    // A save that changes neither server nor proxy does not reconnect.
    let count = saving.asked.lock().unwrap().len();
    backend
        .set_settings(Settings {
            anonymity_target: 7,
            ..chain_settings("http://other.test")
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(saving.asked.lock().unwrap().len(), count);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_chain_server_is_a_problem_and_no_sync() {
    let saving = SavingHarness::new();
    let backend = saving.open().await.unwrap();
    let events = backend.subscribe();
    backend.setup_saving(DEVICE.into()).await.unwrap();
    wait_until("the problem", || async {
        backend.saving_state().await.unwrap().problem.as_deref() == Some(NO_SERVER)
    })
    .await;
    wait_event(&events, &Event::SavingChanged).await;
    let state = backend.saving_state().await.unwrap();
    assert!(!state.chain_online);
    assert!(saving.asked.lock().unwrap().is_empty());
    assert_eq!(saving.chain.scans_made(), 0);

    // Naming one is enough.
    saving.chain.push_full_scan(first_scan(1));
    backend
        .set_settings(chain_settings(CHAIN_URL))
        .await
        .unwrap();
    wait_chain(&*backend, true).await;
    assert_eq!(backend.saving_state().await.unwrap().problem, None);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_server_on_another_chain_points_at_settings() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    saving.chain.push_sync(SyncResponse {
        tx_update: TxUpdate::default(),
        chain_update: Some(CheckPoint::new(BlockId {
            height: 0,
            hash: BlockHash::from_byte_array([9; 32]),
        })),
    });
    sync_saving_now(&backend);
    wait_chain(&*backend, false).await;
    let problem = backend.saving_state().await.unwrap().problem.unwrap();
    assert!(problem.contains("Settings"), "{problem}");
    assert!(!problem.contains("Try again"), "{problem}");
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn jade_is_not_offered_through_a_proxy() {
    let saving = SavingHarness::new();
    saving.devices.add(
        "jade:port",
        "Jade",
        DeviceKind::Jade,
        soft_signer(bitcoin::Network::Regtest),
    );
    let backend = saving.with_server().await;
    let kinds = |found: Vec<fedimix_core::DeviceInfo>| {
        found.into_iter().map(|d| d.kind).collect::<Vec<_>>()
    };
    let listing = backend.detect_devices().await.unwrap();
    assert_eq!(listing.problem, None);
    assert_eq!(
        kinds(listing.devices),
        [DeviceKind::Ledger, DeviceKind::Jade, DeviceKind::Software]
    );
    backend
        .set_settings(Settings {
            socks5_proxy: Some("127.0.0.1:9050".into()),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    let listing = backend.detect_devices().await.unwrap();
    assert_eq!(
        kinds(listing.devices),
        [DeviceKind::Ledger, DeviceKind::Software],
        "the software wallet sends nothing over the network"
    );
    // The Jade left out is said, so the person knows why it is missing.
    assert_eq!(listing.problem.as_deref(), Some(JADE_PROXY));
    assert_eq!(
        backend.setup_saving("jade:port".into()).await,
        Err(FedimixError::NotSetUp(JADE_PROXY.into()))
    );
    assert_eq!(saving.devices.connects(), 0, "the device was not opened");
    assert_eq!(backend.saving_state().await.unwrap().device, None);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_device_shows_the_last_poll() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let connected = || async {
        backend
            .saving_state()
            .await
            .unwrap()
            .device
            .unwrap()
            .connected
    };
    assert!(connected().await);
    let events = backend.subscribe();
    saving.devices.remove(DEVICE);
    assert!(connected().await, "nothing polled yet");
    backend.detect_devices().await.unwrap();
    assert!(!connected().await);
    wait_event(&events, &Event::SavingChanged).await;
    assert_eq!(drain(&events).await, []);
    backend.detect_devices().await.unwrap();
    assert_eq!(drain(&events).await, [], "no change, no event");
    backend.shutdown().await.unwrap();
    drop(backend);

    // After a restart the device is known by name and not seen yet.
    let backend = saving.open().await.unwrap();
    let device = backend.saving_state().await.unwrap().device.unwrap();
    assert_eq!(device.name, "Test device");
    assert!(!device.connected);
    assert!(backend.spending_state().await.unwrap().saving_ready);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_set_up_wallet_syncs_from_open() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let outpoint = saving.fund(&backend, 40_000, 0).await;
    backend.shutdown().await.unwrap();
    drop(backend);
    let made = saving.chain.scans_made();

    let backend = saving.open().await.unwrap();
    assert!(!backend.saving_state().await.unwrap().chain_online);
    assert_eq!(
        backend.saving_state().await.unwrap().coins[0].outpoint,
        outpoint
    );
    wait_chain(&*backend, true).await;
    assert!(saving.chain.scans_made() > made);
    assert_eq!(saving.chain.full_scans_made(), 1, "one first scan for ever");
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn saving_history_pages_over_the_wallet() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    let mut ids = Vec::new();
    for n in 0..3 {
        let outpoint = saving.fund(&backend, 10_000 + u64::from(n), n).await;
        ids.push(outpoint.split(':').next().unwrap().to_string());
    }
    ids.reverse();
    let first = backend.history(WalletKind::Saving, None, 2).await.unwrap();
    let seen: Vec<_> = first.items.iter().map(|i| i.id.clone()).collect();
    assert_eq!(seen, ids[..2]);
    assert!(first.more);
    let last = backend
        .history(WalletKind::Saving, Some(seen[1].clone()), 2)
        .await
        .unwrap();
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.items[0].id, ids[2]);
    assert!(!last.more);
    assert_eq!(
        backend
            .history(WalletKind::Saving, Some("gone".into()), 2)
            .await,
        Err(FedimixError::UnknownItem("gone".into()))
    );
    backend.shutdown().await.unwrap();
}

// ---- Sending from Saving ----

const SAME_ADDRESS: &str =
    "Your last change is still reaching Spending. Try again in a few minutes.";
const ONE_AT_A_TIME: &str =
    "Another Saving payment is still running. Wait for it to end, then try again.";

fn payee() -> String {
    regtest_address("payee")
}

fn saving_send(amount: SendAmount) -> SendRequest {
    SendRequest {
        wallet: WalletKind::Saving,
        destination: payee(),
        amount,
        fee_rate_sat_vb: Some(2),
    }
}

fn saving_sat(sat_amount: u64) -> SendRequest {
    saving_send(SendAmount::Exact(sat(sat_amount)))
}

fn script_of(address: &str) -> ScriptBuf {
    bitcoin::Address::from_str(address)
        .unwrap()
        .assume_checked()
        .script_pubkey()
}

fn paid_to(tx: &Transaction, address: &str) -> Option<u64> {
    let script = script_of(address);
    let out = tx.output.iter().find(|o| o.script_pubkey == script);
    out.map(|o| o.value.to_sat())
}

/// The state of each stored transaction, oldest first.
fn signed_states(saving: &SavingHarness) -> Vec<String> {
    let conn = raw_store(&saving.harness);
    let mut stmt = conn
        .prepare("SELECT state FROM saving_signed ORDER BY state")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get(0)).unwrap();
    rows.map(|row| row.unwrap()).collect()
}

async fn saving_rows(backend: &Real) -> Vec<HistoryItem> {
    backend
        .history(WalletKind::Saving, None, 50)
        .await
        .unwrap()
        .items
}

async fn coin_at(backend: &Real, outpoint: &str) -> Option<fedimix_core::Coin> {
    let state = backend.saving_state().await.unwrap();
    state.coins.into_iter().find(|c| c.outpoint == outpoint)
}

async fn wait_no_saving_operation(backend: &Real) {
    wait_until("the operation to end", || async {
        backend.operations().await.unwrap().is_empty()
    })
    .await;
}

async fn wait_state(backend: &Real, id: &str, state: OperationState) {
    wait_until("the operation's state", || async {
        let running = backend.operations().await.unwrap();
        running.iter().any(|op| op.id == id && op.state == state)
    })
    .await;
}

impl SavingHarness {
    /// Set up and joined. The federation hands out addresses that parse.
    async fn joined(&self) -> Real {
        self.joined_with(DEVICE).await
    }

    /// As [`Self::joined`], set up with the device `id`.
    async fn joined_with(&self, id: &str) -> Real {
        self.harness.first.use_valid_addresses();
        let backend = self.ready_with(id).await;
        backend.join_federation(INVITE.into()).await.unwrap();
        wait_online(&*backend, true).await;
        backend
    }

    /// Confirms `preview` and waits for its end.
    async fn send(&self, backend: &Real, preview: SendPreview) -> (String, OperationState) {
        let events = backend.subscribe();
        let id = backend.confirm_send(preview.id).await.unwrap();
        let (mut states, history_id) = run_of(&events, &id).await;
        let last = states.pop().unwrap();
        if last == OperationState::Done {
            let txid = self.chain.broadcasts().last().unwrap().compute_txid();
            assert_eq!(history_id, Some(txid.to_string()));
        }
        (id, last)
    }

    /// A send of `sat_amount` that ends well, and its transaction.
    async fn sent(&self, backend: &Real, sat_amount: u64) -> Transaction {
        let preview = backend.prepare_send(saving_sat(sat_amount)).await.unwrap();
        let (_, state) = self.send(backend, preview).await;
        assert_eq!(state, OperationState::Done);
        self.chain.broadcasts().last().unwrap().clone()
    }

    /// The next sync, and the one after it, have ended.
    async fn synced(&self, backend: &Real) {
        let start = self.chain.scans_made();
        wait_until("two syncs", || async {
            sync_saving_now(backend);
            self.chain.scans_made() >= start + 2
        })
        .await;
    }

    /// A send that went out, then was lost and refused: stalled. Returns its
    /// transaction and the coin it spends. The server goes on refusing.
    async fn stalled(&self, backend: &Real) -> (Transaction, String) {
        let tx = self.sent(backend, 30_000).await;
        self.chain.script_broadcasts([BroadcastGoes::Refused; 256]);
        self.chain.lose(tx.compute_txid());
        wait_until("the send to stall", || async {
            sync_saving_now(backend);
            signed_states(self).contains(&"stalled".to_string())
        })
        .await;
        (tx.clone(), tx.input[0].previous_output.to_string())
    }

    fn plug_in_jade(&self) {
        self.devices.remove(DEVICE);
        self.devices.add(
            DEVICE,
            "Test device",
            DeviceKind::Jade,
            soft_signer(bitcoin::Network::Regtest),
        );
    }

    /// Replaces the device with one running `signer`.
    fn plug_in(&self, signer: Arc<dyn Signer>) {
        self.devices.remove(DEVICE);
        self.devices
            .add(DEVICE, "Test device", DeviceKind::Ledger, signer);
    }
}

#[tokio::test]
async fn change_goes_to_a_fresh_federation_address() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let first = saving.fund(&backend, 100_000, 0).await;
    let second = saving.fund(&backend, 90_000, 1).await;
    let client = &saving.harness.first;
    let address = regtest_address("first change");
    client.set_deposit_address(Some(address.clone()));

    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    assert_eq!(preview.wallet, WalletKind::Saving);
    assert_eq!(preview.destination, payee());
    assert_eq!(preview.amount, sat(30_000));
    assert_eq!(preview.federation, Some(client.id()));
    let ChangeDestination::Spending { amount: change } = preview.change else {
        panic!("{:?}", preview.change);
    };
    assert_eq!(preview.inputs.len(), 1);
    let input = &preview.inputs[0];
    assert!([&first, &second].contains(&&input.outpoint_or_note));
    assert_eq!(input.tag, Some(PrivacyTag::Unmixed));
    assert_eq!(
        input.amount.as_sat(),
        30_000 + change.as_sat() + preview.fee.as_sat()
    );
    assert!(preview.fee >= sat(2 * 110), "{:?}", preview.fee);

    let (fee, spent_input) = (preview.fee, input.outpoint_or_note.clone());
    let tx = {
        let (_, state) = saving.send(&backend, preview).await;
        assert_eq!(state, OperationState::Done);
        saving.chain.broadcasts()[0].clone()
    };
    assert_eq!(paid_to(&tx, &payee()), Some(30_000));
    assert_eq!(paid_to(&tx, &address), Some(change.as_sat()));
    assert_eq!(tx.output.len(), 2);

    let rows = saving_rows(&backend).await;
    let row = rows.iter().find(|r| r.id == tx.compute_txid().to_string());
    let row = row.unwrap();
    assert_eq!(row.amount, sat(30_000));
    assert_eq!(row.fee, Some(fee));
    assert_eq!(row.status, HistoryStatus::Pending);
    assert_eq!(
        row.kind,
        HistoryKind::Sent {
            change: Some(fedimix_core::SentChange {
                amount: change,
                to_spending: true
            })
        }
    );
    assert_eq!(coin_at(&backend, &spent_input).await, None);

    client.set_deposit_address(None);
    let next = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let (_, state) = saving.send(&backend, next).await;
    assert_eq!(state, OperationState::Done);
    let tx = saving.chain.broadcasts()[1].clone();
    assert_eq!(paid_to(&tx, &address), None, "a fresh address");
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn small_change_stays_and_is_labelled() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let probe = backend.prepare_send(saving_sat(50_000)).await.unwrap();
    let amount = 100_000 - probe.fee.as_sat() - 1_500;
    let preview = backend.prepare_send(saving_sat(amount)).await.unwrap();
    let ChangeDestination::SmallChange { amount: change } = preview.change else {
        panic!("{:?}", preview.change);
    };
    assert!(change <= sat(2_000), "{change:?}");
    let tx = {
        let (_, state) = saving.send(&backend, preview).await;
        assert_eq!(state, OperationState::Done);
        saving.chain.broadcasts()[0].clone()
    };
    let vout = tx
        .output
        .iter()
        .position(|o| o.value.to_sat() == change.as_sat())
        .unwrap();
    let outpoint = format!("{}:{vout}", tx.compute_txid());
    let coin = coin_at(&backend, &outpoint).await.unwrap();
    assert_eq!(coin.tag, PrivacyTag::SmallChange);
    assert!(!coin.confirmed);
    let state = backend.saving_state().await.unwrap();
    assert_eq!(state.balance, change, "own change counts as yours");
    assert_eq!(state.in_flight, Amount::ZERO);
    let rows = saving_rows(&backend).await;
    assert_eq!(rows[0].amount, sat(amount));
    assert_eq!(
        rows[0].kind,
        HistoryKind::Sent {
            change: Some(fedimix_core::SentChange {
                amount: change,
                to_spending: false
            })
        }
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_margin_is_twice_the_minimum_deposit() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let probe = backend.prepare_send(saving_sat(50_000)).await.unwrap();
    let at_margin = 100_000 - probe.fee.as_sat() - 2_000;
    let preview = backend.prepare_send(saving_sat(at_margin)).await.unwrap();
    assert!(
        matches!(preview.change, ChangeDestination::SmallChange { .. }),
        "at the margin it stays: {:?}",
        preview.change
    );
    let above = backend
        .prepare_send(saving_sat(at_margin - 1))
        .await
        .unwrap();
    assert_eq!(
        above.change,
        ChangeDestination::Spending { amount: sat(2_001) }
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn same_v2_address_is_refused() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    let client = &saving.harness.first;
    client.set_deposit_watches(false);
    client.set_deposit_address(Some(regtest_address("repeats")));
    saving.sent(&backend, 30_000).await;
    assert_eq!(
        backend.prepare_send(saving_sat(30_000)).await,
        Err(FedimixError::NotSetUp(SAME_ADDRESS.into()))
    );
    // The scanner moved on: the change was seen.
    client.set_deposit_address(Some(regtest_address("next")));
    saving.sent(&backend, 30_000).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_rejected_send_uses_up_no_address() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving
        .harness
        .first
        .set_deposit_address(Some(regtest_address("repeats")));
    saving.plug_in(Arc::new(
        SoftSigner::from_seed(&SEED, bitcoin::Network::Regtest).rejecting(),
    ));
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    saving.send(&backend, preview).await;
    backend.prepare_send(saving_sat(30_000)).await.unwrap();
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn federation_offline_refuses_before_signing() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let connects = saving.devices.connects();
    saving.harness.first.set_online(false);
    wait_online(&*backend, false).await;
    assert_eq!(
        backend.prepare_send(saving_sat(30_000)).await,
        Err(FedimixError::Offline)
    );
    assert_eq!(
        backend.confirm_send(preview.id).await,
        Err(FedimixError::Offline)
    );
    assert_eq!(saving.devices.connects(), connects, "the device stayed out");
    assert!(saving.chain.broadcasts().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_proxy_keeps_the_change_address_out_of_reach() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    backend
        .set_settings(Settings {
            socks5_proxy: Some("127.0.0.1:9050".into()),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    assert_eq!(
        backend.prepare_send(saving_sat(30_000)).await,
        Err(FedimixError::ProxyUnsupported)
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_federation_keeps_change() {
    let saving = SavingHarness::new();
    let backend = saving.ready().await;
    saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let ChangeDestination::SmallChange { amount: change } = preview.change else {
        panic!("{:?}", preview.change);
    };
    assert!(change > sat(60_000));
    assert!(
        preview.notes.iter().any(|note| note.contains("federation")),
        "{:?}",
        preview.notes
    );
    saving.send(&backend, preview).await;
    let state = backend.saving_state().await.unwrap();
    assert_eq!(state.coins.len(), 1);
    assert_eq!(state.coins[0].tag, PrivacyTag::SmallChange);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_changed_active_federation_expires_the_preview() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.harness.second.use_valid_addresses();
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    backend
        .set_active_federation(saving.harness.second.id())
        .await
        .unwrap();
    assert_eq!(
        backend.confirm_send(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn device_rejects_nothing_broadcasts() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    saving.plug_in(Arc::new(
        SoftSigner::from_seed(&SEED, bitcoin::Network::Regtest).rejecting(),
    ));
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let events = backend.subscribe();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(
        states,
        [
            OperationState::AwaitingDevice,
            OperationState::Signing,
            OperationState::Failed {
                error: FedimixError::DeviceRejected
            }
        ]
    );
    assert!(saving.chain.broadcasts().is_empty());
    assert!(backend.operations().await.unwrap().is_empty());
    assert!(coin_at(&backend, &coin).await.is_some());
    // Usable again.
    saving.plug_in(soft_signer(bitcoin::Network::Regtest));
    saving.sent(&backend, 30_000).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn device_missing_stops_at_awaiting_device_and_cancels() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    saving.devices.remove(DEVICE);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let events = backend.subscribe();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::AwaitingDevice).await;
    backend.cancel_operation(id.clone()).await.unwrap();
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(
        states,
        [OperationState::AwaitingDevice, OperationState::Cancelled]
    );
    assert!(backend.operations().await.unwrap().is_empty());
    assert!(saving.chain.broadcasts().is_empty());
    assert!(coin_at(&backend, &coin).await.is_some());
    backend.prepare_send(saving_sat(30_000)).await.unwrap();
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_device_plugged_in_later_carries_on() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.devices.remove(DEVICE);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let events = backend.subscribe();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::AwaitingDevice).await;
    saving.plug_in(soft_signer(bitcoin::Network::Regtest));
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(states.last(), Some(&OperationState::Done));
    backend.shutdown().await.unwrap();
}

/// Signs, then hands back something else.
struct TamperingSigner {
    inner: SoftSigner,
    tamper: fn(&mut bitcoin::psbt::Psbt),
}

#[async_trait]
impl Signer for TamperingSigner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Software
    }

    async fn fingerprint(&self) -> Result<String> {
        self.inner.fingerprint().await
    }

    async fn xpub(&self, path: &str) -> Result<String> {
        self.inner.xpub(path).await
    }

    async fn sign(&self, psbt: &mut bitcoin::psbt::Psbt) -> Result<()> {
        self.inner.sign(psbt).await?;
        (self.tamper)(psbt);
        Ok(())
    }
}

#[tokio::test]
async fn tampered_psbt_is_refused() {
    let tampers: [fn(&mut bitcoin::psbt::Psbt); 5] = [
        |psbt| psbt.unsigned_tx.input[0].previous_output.vout += 1,
        |psbt| psbt.unsigned_tx.input[0].sequence = Sequence::ZERO,
        |psbt| psbt.unsigned_tx.output[0].value -= bitcoin::Amount::from_sat(1),
        |psbt| psbt.unsigned_tx.output[0].script_pubkey = script_of(&regtest_address("thief")),
        |psbt| {
            let utxo = psbt.inputs[0].witness_utxo.as_mut().unwrap();
            utxo.value += bitcoin::Amount::from_sat(1);
        },
    ];
    for tamper in tampers {
        let saving = SavingHarness::new();
        let backend = saving.joined().await;
        let coin = saving.fund(&backend, 100_000, 0).await;
        saving.plug_in(Arc::new(TamperingSigner {
            inner: SoftSigner::from_seed(&SEED, bitcoin::Network::Regtest),
            tamper,
        }));
        let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
        let (_, state) = saving.send(&backend, preview).await;
        assert_eq!(
            state,
            failed_with(
                "Your device handed back a different transaction from the one you reviewed, so Fedimix did not send it."
            )
        );
        assert!(saving.chain.broadcasts().is_empty());
        assert!(coin_at(&backend, &coin).await.is_some());
        assert_eq!(saving_rows(&backend).await.len(), 1);
        backend.prepare_send(saving_sat(30_000)).await.unwrap();
        backend.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn restart_rebroadcasts_a_signed_tx() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    halt_sends(&backend, SendHalt::BeforeSubmit);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::Broadcasting).await;
    backend.shutdown().await.unwrap();
    drop(backend);
    assert!(saving.chain.broadcasts().is_empty());

    let backend = saving.open().await.unwrap();
    wait_no_saving_operation(&backend).await;
    let broadcasts = saving.chain.broadcasts();
    assert_eq!(broadcasts.len(), 1);
    assert_eq!(paid_to(&broadcasts[0], &payee()), Some(30_000));
    assert_eq!(coin_at(&backend, &coin).await, None);
    assert!(
        saving_rows(&backend)
            .await
            .iter()
            .any(|row| row.txid == Some(broadcasts[0].compute_txid().to_string()))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_recognises_a_seen_tx() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    halt_sends(&backend, SendHalt::AfterSubmit);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_until("the broadcast", || async {
        saving.chain.broadcasts().len() == 1
    })
    .await;
    wait_state(&backend, &id, OperationState::Broadcasting).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    let backend = saving.open().await.unwrap();
    wait_no_saving_operation(&backend).await;
    assert_eq!(saving.chain.broadcasts().len(), 1, "never sent twice");
    assert_eq!(coin_at(&backend, &coin).await, None);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn one_saving_operation_at_a_time() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    let later = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    saving.devices.remove(DEVICE);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::AwaitingDevice).await;
    let busy = Err(FedimixError::NotSetUp(ONE_AT_A_TIME.into()));
    assert_eq!(
        backend.prepare_send(saving_sat(30_000)).await,
        busy.clone().map(|()| unreachable!())
    );
    assert_eq!(
        backend.confirm_send(later.id).await,
        busy.map(|()| unreachable!())
    );
    backend.cancel_operation(id).await.unwrap();
    wait_no_saving_operation(&backend).await;
    backend.prepare_send(saving_sat(30_000)).await.unwrap();
    backend.shutdown().await.unwrap();
}

/// The rule rests on the stored transaction: it needs no memory of the send.
#[tokio::test]
async fn a_stored_tx_keeps_the_rule_across_a_restart() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    saving.chain.set_offline(true);
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::Broadcasting).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    let backend = saving.open().await.unwrap();
    wait_state(&backend, &id, OperationState::Broadcasting).await;
    saving.chain.set_offline(false);
    wait_no_saving_operation(&backend).await;
    assert_eq!(saving.chain.broadcasts().len(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_coins_are_not_spent() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let kept = saving.fund(&backend, 50_000, 0).await;
    let frozen = saving.fund(&backend, 60_000, 1).await;
    backend.set_coin_frozen(frozen.clone(), true).await.unwrap();
    let preview = backend.prepare_send(saving_sat(40_000)).await.unwrap();
    let spent: Vec<_> = preview.inputs.iter().map(|i| &i.outpoint_or_note).collect();
    assert_eq!(spent, [&kept]);
    let (_, state) = saving.send(&backend, preview).await;
    assert_eq!(state, OperationState::Done);
    assert!(coin_at(&backend, &frozen).await.is_some());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn freezing_a_coin_expires_a_preview_that_spends_it() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 50_000, 0).await;
    let preview = backend.prepare_send(saving_sat(40_000)).await.unwrap();
    backend.set_coin_frozen(coin, true).await.unwrap();
    assert_eq!(
        backend.confirm_send(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn above_spendable_is_insufficient() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 50_000, 0).await;
    let frozen = saving.fund(&backend, 60_000, 1).await;
    backend.set_coin_frozen(frozen, true).await.unwrap();
    let reply = backend.prepare_send(saving_sat(55_000)).await;
    let Err(FedimixError::InsufficientFunds { available, needed }) = reply else {
        panic!("{reply:?}");
    };
    assert_eq!(available, sat(50_000), "what can be spent, not the balance");
    assert!(needed > sat(55_000), "with the fee: {needed:?}");
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_preview_explains_a_balance_above_spendable() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 50_000, 0).await;
    let plain = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    assert_eq!(plain.notes, Vec::<String>::new());
    let frozen = saving.fund(&backend, 60_000, 1).await;
    backend.set_coin_frozen(frozen, true).await.unwrap();
    let preview = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    assert_eq!(
        preview.notes,
        [
            "60000 sat in Saving cannot be sent yet: it is frozen, still unconfirmed, or tied to a send that has not settled."
        ]
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn max_sends_everything_minus_fee_without_change() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 50_000, 0).await;
    saving.fund(&backend, 60_000, 1).await;
    let frozen = saving.fund(&backend, 7_000, 2).await;
    backend.set_coin_frozen(frozen, true).await.unwrap();
    let preview = backend
        .prepare_send(saving_send(SendAmount::Max))
        .await
        .unwrap();
    assert_eq!(preview.change, ChangeDestination::None);
    assert_eq!(preview.inputs.len(), 2);
    assert_eq!(preview.amount.as_sat() + preview.fee.as_sat(), 110_000);
    let (amount, _) = (preview.amount, saving.send(&backend, preview).await);
    let tx = saving.chain.broadcasts()[0].clone();
    assert_eq!(tx.output.len(), 1);
    assert_eq!(paid_to(&tx, &payee()), Some(amount.as_sat()));
    let state = backend.saving_state().await.unwrap();
    assert_eq!(state.balance, sat(7_000));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn max_with_nothing_spendable_is_insufficient() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let reply = backend.prepare_send(saving_send(SendAmount::Max)).await;
    assert!(
        matches!(
            reply,
            Err(FedimixError::InsufficientFunds { available, .. }) if available == Amount::ZERO
        ),
        "{reply:?}"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_previews_second_fails() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let first = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let second = backend.prepare_send(saving_sat(20_000)).await.unwrap();
    let (_, state) = saving.send(&backend, first).await;
    assert_eq!(state, OperationState::Done);
    assert_eq!(
        backend.confirm_send(second.id).await,
        Err(FedimixError::PreviewExpired)
    );
    assert_eq!(saving.chain.broadcasts().len(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_send_shows_once_and_enters_the_wallet_at_once() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let tx = saving.sent(&backend, 30_000).await;
    let txid = tx.compute_txid().to_string();
    let rows = saving_rows(&backend).await;
    assert_eq!(
        rows.iter()
            .filter(|r| r.txid.as_ref() == Some(&txid))
            .count(),
        1
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    let state = backend.saving_state().await.unwrap();
    assert_eq!(state.balance, Amount::ZERO, "the change went to Spending");
    let in_store: i64 = raw_store(&saving.harness)
        .query_row(
            "SELECT COUNT(*) FROM history WHERE wallet = 'saving'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(in_store, 0, "the wallet's view is the only list");

    // Mined: the row is final and nothing is left to look after.
    saving.chain.push_sync(confirmed(tx.clone(), 9));
    wait_until("the confirmation", || async {
        sync_saving_now(&backend);
        saving_rows(&backend).await[0].status == HistoryStatus::Complete
    })
    .await;
    saving.synced(&backend).await;
    assert_eq!(signed_states(&saving), ["seen"], "one block can be undone");
    saving.chain.push_sync(SyncResponse {
        chain_update: Some(chain_to(14)),
        ..Default::default()
    });
    wait_until("depth 6", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["confirmed"]
    })
    .await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn broadcast_retries_after_a_dropped_connection() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Offline, BroadcastGoes::Offline]);
    saving.sent(&backend, 30_000).await;
    assert_eq!(saving.chain.broadcasts().len(), 1);
    backend.shutdown().await.unwrap();
}

/// The first attempt reached the server and only its answer was lost: the send
/// has gone out, and saying otherwise would invite a second payment.
#[tokio::test]
async fn a_lost_answer_is_not_a_failure() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.chain.script_broadcasts([BroadcastGoes::AnswerLost]);
    saving.sent(&backend, 30_000).await;
    assert_eq!(
        saving.chain.broadcasts().len(),
        1,
        "seen, so not sent again"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_refusal_ends_the_send_and_frees_what_it_held() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    saving
        .harness
        .first
        .set_deposit_address(Some(regtest_address("repeats")));
    saving.chain.script_broadcasts([BroadcastGoes::Refused]);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let (_, state) = saving.send(&backend, preview).await;
    assert_eq!(
        state,
        OperationState::Failed {
            error: FedimixError::TxRejected("bad-txns-in-belowout".into())
        }
    );
    assert!(coin_at(&backend, &coin).await.is_some());
    assert_eq!(saving_rows(&backend).await.len(), 1);
    // The address was never paid, so it may be used.
    backend.prepare_send(saving_sat(30_000)).await.unwrap();
    backend.shutdown().await.unwrap();
}

/// C2: the attempt with no answer may have delivered it, so a refusal proves
/// nothing, however many follow, and the send cannot be called failed.
#[tokio::test]
async fn a_refusal_after_a_silent_attempt_holds_the_coin() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    let other = saving.fund(&backend, 90_000, 1).await;
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Offline, BroadcastGoes::Refused]);
    saving.chain.script_broadcasts([BroadcastGoes::Refused; 64]);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let spent = preview.inputs[0].outpoint_or_note.clone();
    let (_, state) = saving.send(&backend, preview).await;
    assert_eq!(
        state,
        OperationState::Failed {
            error: FedimixError::SendInDoubt
        }
    );
    assert_eq!(signed_states(&saving), ["stalled"]);
    let rows = saving_rows(&backend).await;
    assert_eq!(rows[0].status, HistoryStatus::Stalled);
    let next = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    assert!(
        next.inputs.iter().any(|i| i.outpoint_or_note == spent),
        "the next send must conflict with it: {:?} of {coin} {other}",
        next.inputs
    );
    backend.shutdown().await.unwrap();
}

/// An answer that judges nothing is no verdict, on a first attempt too.
#[tokio::test]
async fn an_unclear_reply_is_retried() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.chain.script_broadcasts([BroadcastGoes::Unclear]);
    saving.sent(&backend, 30_000).await;
    assert_eq!(saving.chain.broadcasts().len(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_lagging_server_cannot_free_a_signed_send() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let tx = saving.sent(&backend, 30_000).await;
    let txid = tx.compute_txid();

    // The server forgets it, and refuses it once before it takes it back.
    saving.chain.script_broadcasts([BroadcastGoes::Refused]);
    saving.chain.lose(txid);
    wait_until("the rebroadcast", || async {
        sync_saving_now(&backend);
        saving.chain.broadcasts().len() == 2
    })
    .await;
    assert_eq!(saving.chain.broadcasts()[1].compute_txid(), txid);
    wait_until("the send to return", || async {
        saving_rows(&backend)
            .await
            .iter()
            .any(|r| r.txid == Some(txid.to_string()))
    })
    .await;
    backend.shutdown().await.unwrap();
}

/// A `seen` send that no server answers about stays held: its coin is out of
/// selection, whatever the wallet's view says.
#[tokio::test]
async fn a_coin_stays_signed_away_while_the_wallet_says_otherwise() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    let tx = saving.sent(&backend, 30_000).await;
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Offline; 256]);
    saving.chain.lose(tx.compute_txid());
    wait_until("the wallet to let go of the send", || async {
        sync_saving_now(&backend);
        coin_at(&backend, &coin).await.is_some()
    })
    .await;
    saving.synced(&backend).await;
    assert_eq!(signed_states(&saving), ["seen"]);
    let reply = backend.prepare_send(saving_sat(30_000)).await;
    assert!(
        matches!(reply, Err(FedimixError::InsufficientFunds { available, .. }) if available == Amount::ZERO),
        "{reply:?}"
    );
    backend.shutdown().await.unwrap();
}

/// C1: no number of refusals frees a coin. The send stays on show as what it
/// is, and the way out is a next send that conflicts with it.
#[tokio::test]
async fn a_send_the_server_keeps_refusing_stalls_and_stays_held() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let (tx, _) = saving.stalled(&backend).await;
    let start = saving.chain.scans_made();
    wait_until("many refusals", || async {
        sync_saving_now(&backend);
        saving.chain.scans_made() >= start + 12
    })
    .await;
    assert_eq!(signed_states(&saving), ["stalled"]);
    let rows = saving_rows(&backend).await;
    let row = rows
        .iter()
        .find(|r| r.txid == Some(tx.compute_txid().to_string()));
    assert_eq!(row.unwrap().status, HistoryStatus::Stalled);
    assert_eq!(row.unwrap().amount, sat(30_000));

    // Across a restart, with nothing held in memory.
    backend.shutdown().await.unwrap();
    drop(backend);
    let backend = saving.open().await.unwrap();
    assert_eq!(signed_states(&saving), ["stalled"]);
    let rows = saving_rows(&backend).await;
    assert!(rows.iter().any(|r| r.status == HistoryStatus::Stalled));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_next_send_replaces_a_stalled_one() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    let client = &saving.harness.first;
    client.set_deposit_address(Some(regtest_address("repeats")));
    let (old, _) = saving.stalled(&backend).await;
    let spent = old.input[0].previous_output.to_string();

    // v2's repeated address is no bar: only one of the two can pay it.
    let preview = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    assert!(
        preview.inputs.iter().any(|i| i.outpoint_or_note == spent),
        "{:?}",
        preview.inputs
    );
    assert!(
        preview.notes.iter().any(|n| n.contains("only one")),
        "{:?}",
        preview.notes
    );
    // No sync is due now, so nothing can offer the old one in this gap.
    saving.synced(&backend).await;
    saving.chain.clear_broadcast_script();
    let accepted = saving.chain.broadcasts().len();
    let (_, state) = saving.send(&backend, preview).await;
    assert_eq!(state, OperationState::Done);
    let new = saving.chain.broadcasts().last().unwrap().clone();
    assert!(
        new.input
            .iter()
            .any(|i| i.previous_output == old.input[0].previous_output),
        "the two conflict"
    );

    // The old one is not sent again, and is not listed beside its replacement.
    saving.synced(&backend).await;
    assert_eq!(saving.chain.broadcasts().len(), accepted + 1);
    let rows = saving_rows(&backend).await;
    assert!(
        rows.iter().all(|r| r.status != HistoryStatus::Stalled),
        "{rows:?}"
    );

    // Deep enough, the replacement settles the old one for good.
    saving.chain.push_sync(confirmed(new.clone(), 9));
    saving.chain.push_sync(SyncResponse {
        chain_update: Some(chain_to(14)),
        ..Default::default()
    });
    wait_until("both to settle", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["confirmed", "dropped"]
    })
    .await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stalled_send_returns_when_a_server_takes_it() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let (tx, coin) = saving.stalled(&backend).await;
    saving.chain.clear_broadcast_script();
    wait_until("the send to be back", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["seen"]
    })
    .await;
    assert_eq!(coin_at(&backend, &coin).await, None);
    let rows = saving_rows(&backend).await;
    let txid = Some(tx.compute_txid().to_string());
    let row = rows.iter().find(|r| r.txid == txid).unwrap();
    assert_eq!(row.status, HistoryStatus::Pending);
    backend.shutdown().await.unwrap();
}

/// The only proof a send cannot confirm: something else spent its coin, deep.
#[tokio::test]
async fn only_a_deep_conflict_ends_a_stalled_send() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let (tx, _) = saving.stalled(&backend).await;
    let mut conflict = tx.clone();
    conflict.output[0].value -= bitcoin::Amount::from_sat(1);
    saving.chain.push_sync(confirmed(conflict, 9));
    saving.synced(&backend).await;
    assert_eq!(
        signed_states(&saving),
        ["stalled"],
        "one block can be undone"
    );
    saving.chain.push_sync(SyncResponse {
        chain_update: Some(chain_to(14)),
        ..Default::default()
    });
    wait_until("the conflict to count", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["dropped"]
    })
    .await;
    let rows = saving_rows(&backend).await;
    let txid = Some(tx.compute_txid().to_string());
    let row = rows
        .iter()
        .find(|r| r.txid == txid)
        .expect("no row vanishes");
    assert!(
        matches!(&row.status, HistoryStatus::Lost { reason } if reason.contains("another transaction")),
        "{:?}",
        row.status
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_frozen_coin_under_a_stalled_send_is_a_sentence() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    let (_, coin) = saving.stalled(&backend).await;
    backend.set_coin_frozen(coin, true).await.unwrap();
    assert!(matches!(
        backend.prepare_send(saving_sat(1_000)).await,
        Err(FedimixError::NotSetUp(_))
    ));
    backend.shutdown().await.unwrap();
}

/// I4: one confirmation is not the end. A reorg gives the coin back in the
/// wallet's view, so the row must still be there to send it again.
#[tokio::test]
async fn a_reorg_of_a_confirmed_send_rebroadcasts_it() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let tx = saving.sent(&backend, 30_000).await;
    saving.chain.push_sync(confirmed(tx.clone(), 9));
    wait_until("the confirmation", || async {
        sync_saving_now(&backend);
        saving_rows(&backend).await[0].status == HistoryStatus::Complete
    })
    .await;
    saving.synced(&backend).await;
    assert_eq!(signed_states(&saving), ["seen"], "held until depth 6");

    let other = chain_to(8).insert(BlockId {
        height: 9,
        hash: BlockHash::from_byte_array([0xee; 32]),
    });
    let other = other.insert(BlockId {
        height: 10,
        hash: BlockHash::from_byte_array([0xef; 32]),
    });
    saving.chain.lose(tx.compute_txid());
    saving.chain.push_sync(SyncResponse {
        chain_update: Some(other),
        ..Default::default()
    });
    wait_until("the rebroadcast", || async {
        sync_saving_now(&backend);
        saving.chain.broadcasts().len() == 2
    })
    .await;
    backend.shutdown().await.unwrap();
}

/// I5: a rebroadcast under way must not outlive a change of route.
#[tokio::test]
async fn a_proxy_set_during_a_rebroadcast_ends_it() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let tx = saving.sent(&backend, 30_000).await;
    saving.chain.hold_broadcasts(true);
    saving.chain.lose(tx.compute_txid());
    let start = saving.chain.scans_made();
    wait_until("the sync that finds it gone", || async {
        sync_saving_now(&backend);
        saving.chain.scans_made() > start
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    backend
        .set_settings(Settings {
            socks5_proxy: Some("127.0.0.1:9050".into()),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    wait_until("a client on the new route", || async {
        saving
            .last_asked()
            .is_some_and(|(_, proxy)| proxy.is_some())
    })
    .await;
    saving.chain.hold_broadcasts(false);
    backend.shutdown().await.unwrap();
}

/// I2: this would hide where the rest of the coin goes.
#[tokio::test]
async fn paying_the_federations_own_address_is_refused() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let address = regtest_address("repeats");
    saving
        .harness
        .first
        .set_deposit_address(Some(address.clone()));
    let reply = backend
        .prepare_send(SendRequest {
            destination: address,
            ..saving_sat(30_000)
        })
        .await;
    assert!(
        matches!(&reply, Err(FedimixError::InvalidDestination(why)) if why.contains("Refill")),
        "{reply:?}"
    );
    backend.shutdown().await.unwrap();
}

/// I3: the rebuild without the federation must not pick other coins.
#[tokio::test]
async fn large_change_never_stays_as_small_change() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    for (n, amount) in [40_000, 60_000, 90_000, 150_000].into_iter().enumerate() {
        saving.fund(&backend, amount, n as u32).await;
    }
    let probe = backend.prepare_send(saving_sat(10_000)).await.unwrap();
    let amount = 40_000 - probe.fee.as_sat() - 1_500;
    for _ in 0..60 {
        let preview = backend.prepare_send(saving_sat(amount)).await.unwrap();
        if let ChangeDestination::SmallChange { amount } = preview.change {
            assert!(amount <= sat(2_100), "{amount:?} kept as small change");
        }
        backend.discard_preview(preview.id).await.unwrap();
    }
    backend.shutdown().await.unwrap();
}

/// I7: a Max send makes no change, so it needs nothing of the federation.
#[tokio::test]
async fn max_needs_no_federation() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    let client = &saving.harness.first;
    client.set_deposit_address(Some(regtest_address("repeats")));
    saving.sent(&backend, 30_000).await;
    let max = || saving_send(SendAmount::Max);
    backend
        .prepare_send(max())
        .await
        .expect("a recorded address");
    client.set_online(false);
    wait_online(&*backend, false).await;
    let preview = backend.prepare_send(max()).await.expect("offline");
    assert_eq!(preview.federation, None);
    backend
        .set_settings(Settings {
            socks5_proxy: Some("127.0.0.1:9050".into()),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    let preview = backend.prepare_send(max()).await.expect("a proxy");
    let (_, state) = saving.send(&backend, preview).await;
    assert_eq!(state, OperationState::Done);
    backend.shutdown().await.unwrap();
}

/// Holds its signature until told, as a user reading the device does.
struct SlowSigner {
    inner: SoftSigner,
    held: watch::Sender<bool>,
}

#[async_trait]
impl Signer for SlowSigner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Software
    }

    async fn fingerprint(&self) -> Result<String> {
        self.inner.fingerprint().await
    }

    async fn xpub(&self, path: &str) -> Result<String> {
        self.inner.xpub(path).await
    }

    async fn sign(&self, psbt: &mut bitcoin::psbt::Psbt) -> Result<()> {
        let _ = self.held.subscribe().wait_for(|held| !held).await;
        self.inner.sign(psbt).await
    }
}

/// M6: the device can take minutes, and the change must reach the active one.
#[tokio::test]
async fn a_federation_change_during_signing_stops_the_send() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.harness.second.use_valid_addresses();
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    saving.fund(&backend, 100_000, 0).await;
    let signer = Arc::new(SlowSigner {
        inner: SoftSigner::from_seed(&SEED, bitcoin::Network::Regtest),
        held: watch::Sender::new(true),
    });
    saving.plug_in(signer.clone());
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let events = backend.subscribe();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::Signing).await;
    backend
        .set_active_federation(saving.harness.second.id())
        .await
        .unwrap();
    signer.held.send_replace(false);
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(
        states.last(),
        Some(&OperationState::Failed {
            error: FedimixError::PreviewExpired
        })
    );
    assert!(saving.chain.broadcasts().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_an_unknown_saving_id_is_refused() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let id = "saving-0000".to_string();
    assert_eq!(
        backend.cancel_operation(id.clone()).await,
        Err(FedimixError::UnknownItem(id))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_huge_fee_rate_gets_its_own_sentence() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let reply = backend
        .prepare_send(SendRequest {
            fee_rate_sat_vb: Some(u64::MAX),
            ..saving_sat(1_000)
        })
        .await;
    assert!(
        matches!(&reply, Err(FedimixError::InvalidAmount(why)) if why.contains("too high")),
        "{reply:?}"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_jade_behind_a_proxy_is_refused_at_the_review() {
    let saving = SavingHarness::new();
    saving.plug_in_jade();
    let backend = saving.ready().await;
    saving.fund(&backend, 100_000, 0).await;
    backend
        .set_settings(Settings {
            socks5_proxy: Some("127.0.0.1:9050".into()),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    assert_eq!(
        backend.prepare_send(saving_sat(1_000)).await,
        Err(FedimixError::NotSetUp(JADE_PROXY.into()))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn change_reaches_spending_as_a_deposit_row() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let client = &saving.harness.first;
    let tx = saving.sent(&backend, 30_000).await;
    let txid = tx.compute_txid().to_string();
    let watch = client.last_deposit().unwrap();
    client.push_update(
        &watch,
        WatchUpdate::Settled {
            amount: sat(60_000),
            fee: sat(100),
            txid: Some(txid.clone()),
        },
    );
    wait_until("the deposit's row", || async {
        rows(&*backend)
            .await
            .iter()
            .any(|row| row.kind == HistoryKind::Deposit && row.txid.as_ref() == Some(&txid))
    })
    .await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn saving_sends_go_on_chain() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.harness.first.add_invoice("lnbcrt1x", sat(100));
    let reply = backend
        .prepare_send(SendRequest {
            wallet: WalletKind::Saving,
            destination: "lnbcrt1x".into(),
            amount: SendAmount::Exact(sat(100)),
            fee_rate_sat_vb: None,
        })
        .await;
    assert!(reply.is_err(), "{reply:?}");
    let msat = backend
        .prepare_send(saving_send(SendAmount::Exact(Amount { msat: 30_000_500 })))
        .await;
    assert!(
        matches!(msat, Err(FedimixError::InvalidAmount(_))),
        "{msat:?}"
    );
    let dust = backend.prepare_send(saving_sat(10)).await;
    assert!(
        matches!(dust, Err(FedimixError::InvalidAmount(_))),
        "{dust:?}"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_send_needs_the_first_scan() {
    let saving = SavingHarness::new();
    let backend = saving.with_server().await;
    saving.chain.set_stalled(true);
    backend.setup_saving(DEVICE.into()).await.unwrap();
    assert_eq!(
        backend.prepare_send(saving_sat(1_000)).await,
        Err(FedimixError::NotSetUp(SCANNING.into()))
    );
    saving.chain.set_stalled(false);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_estimate_sets_the_fee_and_regtest_has_a_default() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let estimated = SendRequest {
        fee_rate_sat_vb: None,
        ..saving_sat(30_000)
    };
    saving.chain.set_offline(true);
    assert_eq!(
        backend.prepare_send(estimated.clone()).await,
        Err(FedimixError::Offline),
        "no estimate to go on"
    );
    saving.chain.set_offline(false);
    let two = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let default = backend.prepare_send(estimated.clone()).await.unwrap();
    assert_eq!(default.fee, two.fee, "regtest has no estimate: 2 sat/vB");
    saving.chain.set_fee_rate(Some(20.0));
    let high = backend.prepare_send(estimated).await.unwrap();
    assert!(high.fee.as_sat() >= two.fee.as_sat() * 9, "{high:?}");
    backend.shutdown().await.unwrap();
}

/// Polling asks the USB stack, so it happens only while a device can matter.
#[tokio::test]
async fn a_review_polls_the_device_and_nothing_else_does() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let idle = saving.devices.lists();
    tokio::time::sleep(Duration::from_millis(5_200)).await;
    assert_eq!(saving.devices.lists(), idle, "no review, no polling");
    let preview = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    wait_until("a poll", || async { saving.devices.lists() > idle }).await;
    backend.discard_preview(preview.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reviewed = saving.devices.lists();
    tokio::time::sleep(Duration::from_millis(5_200)).await;
    assert_eq!(saving.devices.lists(), reviewed, "the review closed");
    backend.shutdown().await.unwrap();
}

// ---- Replaced sends (fix round 2) ----

/// The stalled send `old`, and a replacement of `sat_amount` that went out.
async fn replaced(
    saving: &SavingHarness,
    backend: &Real,
    sat_amount: u64,
) -> (Transaction, Transaction) {
    let (old, _) = saving.stalled(backend).await;
    let preview = backend.prepare_send(saving_sat(sat_amount)).await.unwrap();
    saving.synced(backend).await;
    saving.chain.clear_broadcast_script();
    let (_, state) = saving.send(backend, preview).await;
    assert_eq!(state, OperationState::Done);
    let new = saving.chain.broadcasts().last().unwrap().clone();
    (old, new)
}

/// N1: the replacement lost. Its row stays, and says which send went instead.
#[tokio::test]
async fn a_replacement_that_loses_says_so() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let (old, new) = replaced(&saving, &backend, 1_000).await;
    let events = backend.subscribe();
    saving.chain.push_sync(confirmed(old.clone(), 9));
    let new_txid = new.compute_txid().to_string();
    let lost = |status: &HistoryStatus| {
        matches!(status, HistoryStatus::Lost { reason }
            if reason.contains("earlier payment")
                && reason.contains("30000 sat")
                && reason.contains(&payee()))
    };
    within("the update of the lost row", async {
        loop {
            sync_saving_now(&backend);
            if let Some(Event::HistoryItemUpdated {
                wallet: WalletKind::Saving,
                item,
            }) = events.next().await
                && item.txid.as_ref() == Some(&new_txid)
            {
                assert!(lost(&item.status), "{:?}", item.status);
                return;
            }
        }
    })
    .await;
    // Final at depth 6, and still on show.
    saving.chain.push_sync(SyncResponse {
        chain_update: Some(chain_to(14)),
        ..Default::default()
    });
    wait_until("the end of both", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["confirmed", "dropped"]
    })
    .await;
    let rows = saving_rows(&backend).await;
    let row = rows.iter().find(|r| r.txid == Some(new_txid.clone()));
    assert!(lost(&row.expect("no row vanishes").status));
    let old_row = rows
        .iter()
        .find(|r| r.txid == Some(old.compute_txid().to_string()));
    assert_eq!(old_row.unwrap().status, HistoryStatus::Complete);
    backend.shutdown().await.unwrap();
}

/// N2: no verdict for long enough stalls the send, which frees nothing and
/// gives the same way out. The clock is the stored row's.
#[tokio::test]
async fn a_send_with_no_verdict_stalls_in_the_end() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Unclear; 256]);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::Broadcasting).await;
    backend.shutdown().await.unwrap();
    drop(backend);
    raw_store(&saving.harness)
        .execute("UPDATE saving_signed SET created_at = 0", [])
        .unwrap();

    let backend = saving.open().await.unwrap();
    wait_no_saving_operation(&backend).await;
    assert_eq!(signed_states(&saving), ["stalled"]);
    let next = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    assert!(next.inputs.iter().any(|i| i.outpoint_or_note == coin));
    backend.shutdown().await.unwrap();
}

/// N3: two `seen` sends sharing a coin, both lost. Only the newest is offered,
/// so the set neither freezes nor revives the send the user replaced.
#[tokio::test]
async fn two_seen_sends_sharing_a_coin_offer_only_the_newest() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let (old, new) = replaced(&saving, &backend, 1_000).await;
    // The old one shows on a server after all.
    // Seen a moment after the replacement, so it is the wallet's pick.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    saving.chain.push_sync(in_mempool(old.clone(), now + 1, 2));
    wait_until("both seen", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["seen", "seen"]
    })
    .await;
    let old_offers = saving.chain.offers_of(old.compute_txid());
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Refused; 256]);
    saving.chain.lose(old.compute_txid());
    saving.chain.lose(new.compute_txid());
    wait_until("the newest to stall", || async {
        sync_saving_now(&backend);
        signed_states(&saving).contains(&"stalled".to_string())
    })
    .await;
    saving.synced(&backend).await;
    assert_eq!(saving.chain.offers_of(old.compute_txid()), old_offers);
    assert!(saving.chain.offers_of(new.compute_txid()) > 1);
    // Not stuck: once the wallet's view has let go of both, which takes a sync
    // each as one hides the other, the next send spends the coin.
    let coin = old.input[0].previous_output.to_string();
    wait_until("a send that spends the coin", || async {
        sync_saving_now(&backend);
        let next = backend.prepare_send(saving_sat(1_000)).await;
        next.is_ok_and(|next| next.inputs.iter().any(|i| i.outpoint_or_note == coin))
    })
    .await;
    assert_eq!(saving.chain.offers_of(old.compute_txid()), old_offers);
    backend.shutdown().await.unwrap();
}

/// N20: two sends of ours on the network that share a coin do not read as two
/// payments: both rows say that at most one of them confirms.
#[tokio::test]
async fn two_seen_sends_sharing_a_coin_read_as_alternatives() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let (old, new) = replaced(&saving, &backend, 1_000).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    saving.chain.push_sync(in_mempool(old.clone(), now + 1, 2));
    wait_until("both seen", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["seen", "seen"]
    })
    .await;
    let ids = [
        old.compute_txid().to_string(),
        new.compute_txid().to_string(),
    ];
    wait_until("both rows contested", || async {
        let rows = saving_rows(&backend).await;
        let shown: Vec<_> = rows
            .iter()
            .filter(|r| r.txid.as_ref().is_some_and(|t| ids.contains(t)))
            .collect();
        shown.len() == 2 && shown.iter().all(|r| r.status == HistoryStatus::Contested)
    })
    .await;
    // The payment that funded Saving is no alternative of anything.
    let rows = saving_rows(&backend).await;
    assert!(
        rows.iter()
            .filter(|r| r.incoming)
            .all(|r| r.status != HistoryStatus::Contested)
    );
    backend.shutdown().await.unwrap();
}

/// N4: a replacement that stalls does not bring back the send it replaced.
#[tokio::test]
async fn a_stalled_replacement_does_not_revive_the_old_send() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let (old, new) = replaced(&saving, &backend, 1_000).await;
    let old_offers = saving.chain.offers_of(old.compute_txid());
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Refused; 256]);
    saving.chain.lose(new.compute_txid());
    wait_until("the replacement to stall", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["stalled", "stalled"]
    })
    .await;
    saving.synced(&backend).await;
    saving.synced(&backend).await;
    assert_eq!(saving.chain.offers_of(old.compute_txid()), old_offers);
    let rows = saving_rows(&backend).await;
    let stalled: Vec<_> = rows
        .iter()
        .filter(|r| r.status == HistoryStatus::Stalled)
        .collect();
    assert_eq!(stalled.len(), 1, "only the replacement is on show");
    assert_eq!(stalled[0].txid, Some(new.compute_txid().to_string()));
    backend.shutdown().await.unwrap();
}

/// N5: a replacement that records no address does not free the old one's.
#[tokio::test]
async fn a_stalled_address_stays_refused_beside_a_max_replacement() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let spare = saving.fund(&backend, 90_000, 1).await;
    saving
        .harness
        .first
        .set_deposit_address(Some(regtest_address("repeats")));
    // Out of reach of the stalled send and of the Max replacement.
    backend.set_coin_frozen(spare.clone(), true).await.unwrap();
    let (_, _) = saving.stalled(&backend).await;
    let max = backend
        .prepare_send(saving_send(SendAmount::Max))
        .await
        .unwrap();
    saving.synced(&backend).await;
    saving.chain.clear_broadcast_script();
    assert_eq!(saving.send(&backend, max).await.1, OperationState::Done);
    backend.set_coin_frozen(spare, false).await.unwrap();
    assert_eq!(
        backend.prepare_send(saving_sat(30_000)).await,
        Err(FedimixError::NotSetUp(SAME_ADDRESS.into()))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stalled_address_stays_refused_beside_a_small_change_replacement() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let first = saving.fund(&backend, 100_000, 0).await;
    let spare = saving.fund(&backend, 90_000, 1).await;
    let client = &saving.harness.first;
    client.set_deposit_address(Some(regtest_address("repeats")));
    // The stalled send and its replacement spend the first coin only.
    backend.set_coin_frozen(spare.clone(), true).await.unwrap();
    let (old, _) = saving.stalled(&backend).await;
    assert_eq!(old.input[0].previous_output.to_string(), first);
    let probe = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    assert_eq!(probe.inputs.len(), 1);
    let amount = 100_000 - probe.fee.as_sat() - 1_500;
    let small = backend.prepare_send(saving_sat(amount)).await.unwrap();
    assert!(
        matches!(small.change, ChangeDestination::SmallChange { .. }),
        "{:?}",
        small.change
    );
    saving.synced(&backend).await;
    saving.chain.clear_broadcast_script();
    assert_eq!(saving.send(&backend, small).await.1, OperationState::Done);
    backend.set_coin_frozen(spare, false).await.unwrap();
    assert_eq!(
        backend.prepare_send(saving_sat(30_000)).await,
        Err(FedimixError::NotSetUp(SAME_ADDRESS.into()))
    );
    backend.shutdown().await.unwrap();
}

/// A first refusal on fee grounds: a node that relays more may still take it.
#[tokio::test]
async fn a_fee_refusal_holds_the_coin() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    saving.chain.script_broadcasts([BroadcastGoes::LowFee; 256]);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let (_, state) = saving.send(&backend, preview).await;
    // Never "refused": Fedimix keeps offering it, so the user is told so.
    assert_eq!(
        state,
        OperationState::Failed {
            error: FedimixError::SendHeld("mempool min fee not met".into())
        }
    );
    assert_eq!(signed_states(&saving), ["stalled"]);
    let next = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    assert!(next.inputs.iter().any(|i| i.outpoint_or_note == coin));
    backend.shutdown().await.unwrap();
}

/// N12: a stalled send of several coins makes the next send spend them all.
#[tokio::test]
async fn a_stalled_send_of_several_coins_holds_them_all() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 20_000, 0).await;
    saving.fund(&backend, 20_000, 1).await;
    let tx = {
        let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
        assert_eq!(preview.inputs.len(), 2);
        saving.send(&backend, preview).await;
        saving.chain.broadcasts().last().unwrap().clone()
    };
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Refused; 256]);
    saving.chain.lose(tx.compute_txid());
    wait_until("the send to stall", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["stalled"]
    })
    .await;
    let next = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    for input in &tx.input {
        let coin = input.previous_output.to_string();
        assert!(next.inputs.iter().any(|i| i.outpoint_or_note == coin));
    }
    backend.shutdown().await.unwrap();
}

/// N12: a restart between the replacement's row and its broadcast resumes the
/// replacement only.
#[tokio::test]
async fn a_restart_mid_replacement_resumes_only_the_replacement() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let (old, _) = saving.stalled(&backend).await;
    let preview = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    saving.synced(&backend).await;
    halt_sends(&backend, SendHalt::BeforeSubmit);
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::Broadcasting).await;
    backend.shutdown().await.unwrap();
    drop(backend);
    let old_offers = saving.chain.offers_of(old.compute_txid());
    saving.chain.clear_broadcast_script();

    let backend = saving.open().await.unwrap();
    wait_no_saving_operation(&backend).await;
    saving.synced(&backend).await;
    assert_eq!(saving.chain.offers_of(old.compute_txid()), old_offers);
    assert_eq!(signed_states(&saving), ["seen", "stalled"]);
    backend.shutdown().await.unwrap();
}

// ---- Fix round 3 ----

const OLD_BACK: &str = "An earlier payment is back on the network while the payment that replaced it has not confirmed. Wait until one of them confirms, and up to 6 blocks after that, before sending from Saving again. Fedimix cannot raise the fee to make either confirm sooner.";

/// The stored state of `tx`.
fn state_of(saving: &SavingHarness, tx: &Transaction) -> String {
    raw_store(&saving.harness)
        .query_row(
            "SELECT state FROM saving_signed WHERE txid = ?1",
            [tx.compute_txid().to_string()],
            |r| r.get(0),
        )
        .unwrap()
}

/// The stalled sends now, as transactions.
fn stalled_txs(saving: &SavingHarness) -> Vec<Transaction> {
    let conn = raw_store(&saving.harness);
    let mut stmt = conn
        .prepare("SELECT tx_hex FROM saving_signed WHERE state = 'stalled'")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|hex| bitcoin::consensus::encode::deserialize_hex(&hex.unwrap()).unwrap())
        .collect()
}

/// The invariant behind every replacement: a send signed while others are
/// stalled shares a coin with each, so of two that pay one recipient with no
/// coin in common, never both can confirm. Checked at every confirm.
async fn send_checked(
    saving: &SavingHarness,
    backend: &Real,
    preview: SendPreview,
) -> OperationState {
    let stalled = stalled_txs(saving);
    let before = saving.chain.broadcasts().len();
    let (_, state) = saving.send(backend, preview).await;
    conflicts_with_each(saving, &stalled, before, &state);
    state
}

/// [`send_checked`] for a refill, which is a payment too.
async fn refill_checked(
    saving: &SavingHarness,
    backend: &Real,
    preview: RefillPreview,
) -> OperationState {
    let stalled = stalled_txs(saving);
    let before = saving.chain.broadcasts().len();
    let (_, state) = saving.refill(backend, preview).await;
    conflicts_with_each(saving, &stalled, before, &state);
    state
}

/// The newest broadcast, if one was made, shares a coin with each of `stalled`.
fn conflicts_with_each(
    saving: &SavingHarness,
    stalled: &[Transaction],
    before: usize,
    state: &OperationState,
) {
    if *state == OperationState::Done && saving.chain.broadcasts().len() > before {
        let new = saving.chain.broadcasts().last().unwrap().clone();
        for old in stalled {
            assert!(
                new.input.iter().any(|i| old
                    .input
                    .iter()
                    .any(|o| o.previous_output == i.previous_output)),
                "a payment shares no coin with a stalled one"
            );
        }
    }
}

/// N13: the replaced send is back in view and its stalled replacement's coin
/// is spent there. No next send can conflict with the replacement, so none is
/// made until one of the two confirms.
#[tokio::test]
async fn a_stalled_replacement_beside_its_old_send_in_view_holds_saving() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    let (old, new) = replaced(&saving, &backend, 1_000).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Back on the network: no later sync may report it evicted again, which
    // a slow run reached once a second had passed.
    saving.chain.find(old.compute_txid());
    saving.chain.push_sync(in_mempool(old.clone(), now + 1, 3));
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Refused; 256]);
    saving.chain.lose(new.compute_txid());
    wait_until(
        "the old send in view and the replacement stalled",
        || async {
            sync_saving_now(&backend);
            state_of(&saving, &old) == "seen" && state_of(&saving, &new) == "stalled"
        },
    )
    .await;
    let reply = backend.prepare_send(saving_sat(1_000)).await;
    assert_eq!(reply, Err(FedimixError::NotSetUp(OLD_BACK.into())));
    backend.shutdown().await.unwrap();
}

/// The invariant over the scenarios of this file: a plain stall, a stall of
/// several coins, a replacement that stalls in turn, and a Max replacement.
#[tokio::test]
async fn every_send_conflicts_with_every_stalled_one() {
    // A plain stall, then two replacements, the first of which stalls.
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    saving.fund(&backend, 80_000, 2).await;
    let (_, _) = saving.stalled(&backend).await;
    for amount in [1_000, 2_000] {
        let preview = backend.prepare_send(saving_sat(amount)).await.unwrap();
        saving.synced(&backend).await;
        saving.chain.clear_broadcast_script();
        assert_eq!(
            send_checked(&saving, &backend, preview).await,
            OperationState::Done
        );
        let last = saving.chain.broadcasts().last().unwrap().compute_txid();
        saving
            .chain
            .script_broadcasts([BroadcastGoes::Refused; 256]);
        saving.chain.lose(last);
        wait_until("the replacement to stall", || async {
            sync_saving_now(&backend);
            stalled_txs(&saving)
                .iter()
                .any(|tx| tx.compute_txid() == last)
        })
        .await;
    }
    // Max replaces them all.
    let max = backend
        .prepare_send(saving_send(SendAmount::Max))
        .await
        .unwrap();
    saving.synced(&backend).await;
    saving.chain.clear_broadcast_script();
    assert_eq!(
        send_checked(&saving, &backend, max).await,
        OperationState::Done
    );
    backend.shutdown().await.unwrap();

    // Several coins in one stalled send.
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 20_000, 0).await;
    saving.fund(&backend, 20_000, 1).await;
    saving.fund(&backend, 50_000, 2).await;
    backend
        .set_coin_frozen(saving_coin(&backend, 50_000).await, true)
        .await
        .unwrap();
    let tx = saving.sent(&backend, 30_000).await;
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Refused; 256]);
    saving.chain.lose(tx.compute_txid());
    wait_until("the send to stall", || async {
        sync_saving_now(&backend);
        signed_states(&saving) == ["stalled"]
    })
    .await;
    backend
        .set_coin_frozen(saving_coin(&backend, 50_000).await, false)
        .await
        .unwrap();
    let preview = backend.prepare_send(saving_sat(45_000)).await.unwrap();
    saving.synced(&backend).await;
    saving.chain.clear_broadcast_script();
    assert_eq!(
        send_checked(&saving, &backend, preview).await,
        OperationState::Done
    );
    backend.shutdown().await.unwrap();

    // A refill joins the net: it goes while nothing is stalled, a send then
    // replaces it once it stalls, and no refill runs beside it.
    let saving = SavingHarness::new();
    let backend = saving.refillable(20_000).await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    saving.fund(&backend, 80_000, 2).await;
    let preview = backend.prepare_refill().await.unwrap();
    assert_eq!(
        refill_checked(&saving, &backend, preview).await,
        OperationState::Done
    );
    let refill = saving.chain.broadcasts().last().unwrap().compute_txid();
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Refused; 256]);
    saving.chain.lose(refill);
    wait_until("the refill to stall", || async {
        sync_saving_now(&backend);
        stalled_txs(&saving)
            .iter()
            .any(|tx| tx.compute_txid() == refill)
    })
    .await;
    assert_eq!(
        backend.prepare_refill().await.map(|_| ()),
        Err(FedimixError::NotSetUp(REFILL_STALLED.into()))
    );
    let preview = backend.prepare_send(saving_sat(1_000)).await.unwrap();
    saving.synced(&backend).await;
    saving.chain.clear_broadcast_script();
    assert_eq!(
        send_checked(&saving, &backend, preview).await,
        OperationState::Done
    );
    // The replaced refill never settles: the block names what lifts it.
    saving.synced(&backend).await;
    assert_eq!(
        backend.prepare_refill().await.map(|_| ()),
        Err(FedimixError::NotSetUp(REFILL_REPLACED.into()))
    );
    backend.shutdown().await.unwrap();
}

async fn saving_coin(backend: &Real, sat_amount: u64) -> String {
    let state = backend.saving_state().await.unwrap();
    let coin = state.coins.iter().find(|c| c.amount == sat(sat_amount));
    coin.unwrap().outpoint.clone()
}

/// N16: a send reported `Done` that left the wallet's view while no server
/// answers keeps its row, as pending.
#[tokio::test]
async fn a_seen_send_with_no_answer_keeps_its_row() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    let tx = saving.sent(&backend, 30_000).await;
    saving
        .chain
        .script_broadcasts([BroadcastGoes::Offline; 256]);
    saving.chain.lose(tx.compute_txid());
    wait_until("the wallet to let go of it", || async {
        sync_saving_now(&backend);
        coin_at(&backend, &coin).await.is_some()
    })
    .await;
    let rows = saving_rows(&backend).await;
    let row = rows
        .iter()
        .find(|r| r.txid == Some(tx.compute_txid().to_string()));
    assert_eq!(row.expect("the row stays").status, HistoryStatus::Pending);
    backend.shutdown().await.unwrap();
}

// Refill (Task 7).

const AT_TARGET: &str = "Spending is at or above its target, so there is nothing to refill.";
const ON_ITS_WAY: &str = "Spending reaches its target with the money already on its way to it, so there is nothing to refill.";
const REFILL_PENDING: &str = "Your last refill is not yet 6 blocks deep. Refill again once it is.";
const REFILL_STALLED: &str = "No chain server holds your last refill now, and it may still go through. Refill again once it is 6 blocks deep, or once a send from Saving that replaces it is.";
const REFILL_REPLACED: &str =
    "A send from Saving replaced your last refill. Refill again once that send is 6 blocks deep.";
/// The fake federation's minimum deposit, which is also its deposit fee.
const DEPOSIT_FEE: u64 = 1_000;

impl SavingHarness {
    /// Joined, with Spending empty and a target of `target` sat.
    async fn refillable(&self, target: u64) -> Real {
        let backend = self.joined().await;
        backend
            .set_settings(Settings {
                spending_target: sat(target),
                ..chain_settings(CHAIN_URL)
            })
            .await
            .unwrap();
        backend
    }

    /// Confirms `preview` and waits for its end, every event a refill's.
    async fn refill(&self, backend: &Real, preview: RefillPreview) -> (String, OperationState) {
        let events = backend.subscribe();
        let id = backend.confirm_refill(preview.id).await.unwrap();
        let (mut states, history_id) = run_of_kind(&events, &id, OperationKind::Refill).await;
        let last = states.pop().unwrap();
        if last == OperationState::Done {
            let txid = self.chain.broadcasts().last().unwrap().compute_txid();
            assert_eq!(history_id, Some(txid.to_string()));
        }
        (id, last)
    }

    /// A refill that ends well, and its transaction.
    async fn refilled(&self, backend: &Real) -> Transaction {
        let preview = backend.prepare_refill().await.unwrap();
        let (_, state) = self.refill(backend, preview).await;
        assert_eq!(state, OperationState::Done);
        self.chain.broadcasts().last().unwrap().clone()
    }
}

fn outpoints(preview: &RefillPreview) -> Vec<String> {
    preview.coins.iter().map(|c| c.outpoint.clone()).collect()
}

#[tokio::test]
async fn smallest_first_until_covered() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(25_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let ten = saving.fund(&backend, 10_000, 1).await;
    let thirty = saving.fund(&backend, 30_000, 2).await;
    let twenty = saving.fund(&backend, 20_000, 3).await;
    let preview = backend.prepare_refill().await.unwrap();
    assert_eq!(outpoints(&preview), [ten.clone(), twenty.clone()]);
    assert_eq!(preview.amount, sat(30_000));
    assert_eq!(preview.device_name, "Test device");
    // The gap of 25k and the deposit fee of 1k are covered by 30k less the
    // fee. 30k of coins cover a gap of 30k only before the fees: one more coin.
    backend
        .set_settings(Settings {
            spending_target: sat(30_000),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    let preview = backend.prepare_refill().await.unwrap();
    assert_eq!(outpoints(&preview), [ten, twenty, thirty]);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn delivered_is_net_of_fee() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(25_000).await;
    saving.harness.first.set_balance(sat(5_000));
    wait_balance(&*backend, sat(5_000)).await;
    saving.fund(&backend, 50_000, 0).await;
    let preview = backend.prepare_refill().await.unwrap();
    assert_eq!(preview.amount, sat(50_000));
    assert!(preview.fee > Amount::ZERO);
    let delivered = preview.amount.checked_sub(preview.fee).unwrap();
    // The gap of 20k, and the federation's deposit fee, so Spending lands at
    // or above the target.
    let needs = sat(20_000 + DEPOSIT_FEE);
    assert_eq!(preview.overshoot, delivered.checked_sub(needs).unwrap());
    let fee = preview.fee;
    let tx = saving.refilled(&backend).await;
    assert_eq!(tx.output.len(), 1);
    assert_eq!(sat(tx.output[0].value.to_sat()), delivered);
    assert_eq!(sat(50_000 - tx.output[0].value.to_sat()), fee);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn one_output_no_change() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    let address = regtest_address("refill");
    saving
        .harness
        .first
        .set_deposit_address(Some(address.clone()));
    let spent = saving.fund(&backend, 50_000, 0).await;
    let kept = saving.fund(&backend, 60_000, 1).await;
    let preview = backend.prepare_refill().await.unwrap();
    assert_eq!(outpoints(&preview), [spent]);
    let delivered = preview.amount.checked_sub(preview.fee).unwrap();
    let tx = saving.refilled(&backend).await;
    assert_eq!(tx.output.len(), 1, "no change output");
    assert_eq!(paid_to(&tx, &address).map(sat), Some(delivered));
    let state = backend.saving_state().await.unwrap();
    let coins: Vec<_> = state.coins.iter().map(|c| c.outpoint.clone()).collect();
    assert_eq!(coins, [kept]);
    let txid = tx.compute_txid().to_string();
    let rows = saving_rows(&backend).await;
    let row = rows
        .iter()
        .find(|r| r.txid.as_ref() == Some(&txid))
        .unwrap();
    assert_eq!(row.kind, HistoryKind::Refill, "a refill, never change");
    assert_eq!(row.amount, delivered);
    assert!(!row.incoming);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn at_or_above_target_is_refused() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(50_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let refused = Err(FedimixError::InvalidAmount(AT_TARGET.into()));
    for balance in [50_000, 60_000] {
        saving.harness.first.set_balance(sat(balance));
        wait_balance(&*backend, sat(balance)).await;
        assert_eq!(backend.prepare_refill().await.map(|_| ()), refused);
    }
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn not_enough_coins_is_insufficient() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(100_000).await;
    saving.fund(&backend, 10_000, 0).await;
    saving.fund(&backend, 20_000, 1).await;
    let reply = backend.prepare_refill().await;
    assert!(
        matches!(reply, Err(FedimixError::InsufficientFunds { .. })),
        "{reply:?}"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_and_unconfirmed_are_skipped() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(10_000).await;
    let big = saving.fund(&backend, 20_000, 0).await;
    let frozen = saving.fund(&backend, 5_000, 1).await;
    backend.set_coin_frozen(frozen, true).await.unwrap();
    let address = backend.receive(on_chain()).await.unwrap().payload;
    let tx = payment(&address, 8_000, 2);
    let unconfirmed = format!("{}:0", tx.compute_txid());
    saving.chain.push_sync(in_mempool(tx, 2_000, 3));
    sync_saving_now(&backend);
    wait_until("the unconfirmed coin", || async {
        coin_at(&backend, &unconfirmed).await.is_some()
    })
    .await;
    let preview = backend.prepare_refill().await.unwrap();
    assert_eq!(outpoints(&preview), [big]);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn refill_needs_the_federation_online() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_refill().await.unwrap();
    let connects = saving.devices.connects();
    saving.harness.first.set_online(false);
    wait_online(&*backend, false).await;
    assert_eq!(
        backend.prepare_refill().await.map(|_| ()),
        Err(FedimixError::Offline)
    );
    assert_eq!(
        backend.confirm_refill(preview.id).await,
        Err(FedimixError::Offline)
    );
    assert_eq!(saving.devices.connects(), connects, "the device stayed out");
    assert!(saving.chain.broadcasts().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn refill_same_v2_address_is_refused() {
    let saving = SavingHarness::new();
    // Above what the send's change brings on its way.
    let backend = saving.refillable(200_000).await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    saving.fund(&backend, 80_000, 2).await;
    let client = &saving.harness.first;
    client.set_deposit_watches(false);
    client.set_deposit_address(Some(regtest_address("repeats")));
    saving.sent(&backend, 30_000).await;
    let same = Err(FedimixError::NotSetUp(SAME_ADDRESS.into()));
    assert_eq!(backend.prepare_refill().await.map(|_| ()), same);
    // The scanner moved on; the refill's own address then holds the next one.
    client.set_deposit_address(Some(regtest_address("next")));
    saving.refilled(&backend).await;
    assert_eq!(
        backend.prepare_send(saving_sat(30_000)).await.map(|_| ()),
        same
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn device_rejection_changes_nothing() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    saving.plug_in(Arc::new(
        SoftSigner::from_seed(&SEED, bitcoin::Network::Regtest).rejecting(),
    ));
    let preview = backend.prepare_refill().await.unwrap();
    let (_, state) = saving.refill(&backend, preview).await;
    assert_eq!(
        state,
        OperationState::Failed {
            error: FedimixError::DeviceRejected
        }
    );
    assert!(saving.chain.broadcasts().is_empty());
    assert!(backend.operations().await.unwrap().is_empty());
    assert!(signed_states(&saving).is_empty());
    assert!(coin_at(&backend, &coin).await.is_some());
    assert!(saving_rows(&backend).await.iter().all(|r| r.incoming));
    // Usable again.
    saving.plug_in(soft_signer(bitcoin::Network::Regtest));
    saving.refilled(&backend).await;
    backend.shutdown().await.unwrap();
}

/// A refill taken up after a restart is still a refill, in `operations` and in
/// its events.
#[tokio::test]
async fn a_resumed_refill_reports_as_a_refill() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_refill().await.unwrap();
    saving.chain.set_offline(true);
    let id = backend.confirm_refill(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::Broadcasting).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    let backend = saving.open().await.unwrap();
    let running = backend.operations().await.unwrap();
    assert_eq!(
        running,
        [RunningOperation {
            id: id.clone(),
            kind: OperationKind::Refill,
            state: OperationState::Broadcasting,
        }]
    );
    let events = backend.subscribe();
    saving.chain.set_offline(false);
    let (states, _) = run_of_kind(&events, &id, OperationKind::Refill).await;
    assert_eq!(states.last(), Some(&OperationState::Done));
    backend.shutdown().await.unwrap();
}

/// The refill's deposit reaches Spending as a Deposit row of its transaction.
#[tokio::test]
async fn a_refill_reaches_spending_as_a_deposit_row() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let client = &saving.harness.first;
    let tx = saving.refilled(&backend).await;
    let txid = tx.compute_txid().to_string();
    let watch = client.last_deposit().unwrap();
    client.push_update(
        &watch,
        WatchUpdate::Settled {
            amount: sat(tx.output[0].value.to_sat()),
            fee: sat(100),
            txid: Some(txid.clone()),
        },
    );
    wait_until("the deposit's row", || async {
        rows(&*backend)
            .await
            .iter()
            .any(|row| row.kind == HistoryKind::Deposit && row.txid.as_ref() == Some(&txid))
    })
    .await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_refill_waits_for_a_running_send() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    let refill = backend.prepare_refill().await.unwrap();
    saving.devices.remove(DEVICE);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    let id = backend.confirm_send(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::AwaitingDevice).await;
    let busy = Err(FedimixError::NotSetUp(ONE_AT_A_TIME.into()));
    assert_eq!(backend.prepare_refill().await.map(|_| ()), busy);
    assert_eq!(backend.confirm_refill(refill.id).await.map(|_| ()), busy);
    backend.cancel_operation(id).await.unwrap();
    wait_no_saving_operation(&backend).await;
    backend.shutdown().await.unwrap();
}

/// A refill preview is not a send preview, and the other way round.
#[tokio::test]
async fn a_refill_preview_confirms_only_as_a_refill() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let refill = backend.prepare_refill().await.unwrap();
    assert_eq!(
        backend.confirm_send(refill.id.clone()).await,
        Err(FedimixError::UnknownItem(refill.id.clone()))
    );
    let send = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    assert_eq!(
        backend.confirm_refill(send.id.clone()).await,
        Err(FedimixError::UnknownItem(send.id.clone()))
    );
    backend.shutdown().await.unwrap();
}

/// A refill on its way is money on its way: no second one runs beside it.
#[tokio::test]
async fn a_refill_waits_for_the_last_one() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    saving.fund(&backend, 80_000, 2).await;
    saving.refilled(&backend).await;
    assert_eq!(
        backend.prepare_refill().await.map(|_| ()),
        Err(FedimixError::NotSetUp(REFILL_PENDING.into()))
    );
    assert_eq!(saving.chain.broadcasts().len(), 1);
    backend.shutdown().await.unwrap();
}

/// A refill would cancel a stalled payment to someone else: it is refused,
/// naming that payment, which stays in the history.
#[tokio::test]
async fn a_refill_never_cancels_a_stalled_send() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(10_000).await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 5_000, 1).await;
    let (old, _) = saving.stalled(&backend).await;
    let reply = backend.prepare_refill().await.map(|_| ());
    let Err(FedimixError::NotSetUp(sentence)) = reply else {
        panic!("{reply:?}");
    };
    assert!(sentence.contains(&payee()), "{sentence}");
    assert!(sentence.contains(&sat(30_000).to_string()), "{sentence}");
    let txid = old.compute_txid().to_string();
    let rows = saving_rows(&backend).await;
    let row = rows.iter().find(|r| r.txid.as_ref() == Some(&txid));
    assert_eq!(row.expect("the send stays").status, HistoryStatus::Stalled);
    assert_eq!(saving.chain.broadcasts().len(), 1);
    backend.shutdown().await.unwrap();
}

/// Change on its way to Spending counts toward the target, less its deposit
/// fee, and so does the refill's own deposit fee.
#[tokio::test]
async fn change_on_its_way_counts_toward_the_target() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(100_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_send(saving_sat(39_000)).await.unwrap();
    let ChangeDestination::Spending { amount: change } = preview.change else {
        panic!("{:?}", preview.change);
    };
    let (_, state) = saving.send(&backend, preview).await;
    assert_eq!(state, OperationState::Done);
    saving.fund(&backend, 200_000, 1).await;
    let refill = backend.prepare_refill().await.unwrap();
    let delivered = refill.amount.checked_sub(refill.fee).unwrap();
    let arrives = change.checked_sub(sat(DEPOSIT_FEE)).unwrap();
    let needs = sat(100_000)
        .checked_sub(arrives)
        .and_then(|gap| gap.checked_add(sat(DEPOSIT_FEE)))
        .unwrap();
    assert_eq!(delivered.checked_sub(refill.overshoot), Some(needs));
    // A lower target the change alone reaches: refused before an address is
    // used up.
    backend
        .set_settings(Settings {
            spending_target: arrives,
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    let last_address = saving.harness.first.last_deposit();
    assert_eq!(
        backend.prepare_refill().await.map(|_| ()),
        Err(FedimixError::InvalidAmount(ON_ITS_WAY.into()))
    );
    assert_eq!(saving.harness.first.last_deposit(), last_address);
    backend.shutdown().await.unwrap();
}

/// What a refill must deliver, read back from its preview.
fn needs_of(preview: &RefillPreview) -> Amount {
    let delivered = preview.amount.checked_sub(preview.fee).unwrap();
    delivered.checked_sub(preview.overshoot).unwrap()
}

/// Sends 39k of a 100k coin with change to Spending, funds a 200k coin to
/// refill from, and returns the change, its deposit watch and the txid.
async fn change_on_its_way(saving: &SavingHarness, backend: &Real) -> (Amount, String, String) {
    saving.fund(backend, 100_000, 0).await;
    let preview = backend.prepare_send(saving_sat(39_000)).await.unwrap();
    let ChangeDestination::Spending { amount: change } = preview.change else {
        panic!("{:?}", preview.change);
    };
    let (_, state) = saving.send(backend, preview).await;
    assert_eq!(state, OperationState::Done);
    let watch = saving.harness.first.last_deposit().unwrap();
    let txid = saving.chain.broadcasts().last().unwrap().compute_txid();
    saving.fund(backend, 200_000, 1).await;
    (change, watch, txid.to_string())
}

/// What a 100k target needs with `change` on its way: the gap left after the
/// change, less its deposit fee, plus the refill's own deposit fee.
fn needs_beside(change: Amount) -> Amount {
    let arrives = change.checked_sub(sat(DEPOSIT_FEE)).unwrap();
    sat(100_000 + DEPOSIT_FEE).checked_sub(arrives).unwrap()
}

/// v1: the deposit row of change confirming names its transaction and shows
/// what arrived on chain; it is counted once, net of the deposit fee.
#[tokio::test]
async fn a_v1_deposit_confirming_counts_once() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(100_000).await;
    let (change, watch, txid) = change_on_its_way(&saving, &backend).await;
    assert_eq!(
        needs_of(&backend.prepare_refill().await.unwrap()),
        needs_beside(change)
    );
    saving.harness.first.push_update(
        &watch,
        WatchUpdate::Confirming {
            confirmations: 0,
            required: 11,
            amount: change,
            txid: Some(txid.clone()),
        },
    );
    wait_until("the confirming row", || async {
        rows(&*backend)
            .await
            .iter()
            .any(|r| r.kind == HistoryKind::Deposit && r.txid.as_ref() == Some(&txid))
    })
    .await;
    assert_eq!(
        needs_of(&backend.prepare_refill().await.unwrap()),
        needs_beside(change)
    );
    backend.shutdown().await.unwrap();
}

/// v2: the pending deposit row of change names its transaction and shows what
/// arrives; it is counted once.
#[tokio::test]
async fn a_v2_deposit_pending_counts_once() {
    let saving = SavingHarness::new();
    let client = &saving.harness.first;
    client.set_deposit_watches(false);
    let backend = saving.refillable(100_000).await;
    let (change, _, txid) = change_on_its_way(&saving, &backend).await;
    let fee = sat(DEPOSIT_FEE);
    client.push_deposit(DepositEvent {
        cursor: 1,
        id: format!("{txid}:1"),
        amount: change.checked_sub(fee).unwrap(),
        fee,
        txid: Some(txid.clone()),
        confirmed: false,
    });
    wait_until("the pending row", || async {
        rows(&*backend)
            .await
            .iter()
            .any(|r| r.kind == HistoryKind::Deposit && r.txid.as_ref() == Some(&txid))
    })
    .await;
    assert_eq!(
        needs_of(&backend.prepare_refill().await.unwrap()),
        needs_beside(change)
    );
    backend.shutdown().await.unwrap();
}

/// Money on its way to one federation does not fill another.
#[tokio::test]
async fn money_on_its_way_counts_for_its_own_federation() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(100_000).await;
    // Its own address: the fakes number their addresses alike.
    let second = &saving.harness.second;
    second.set_deposit_address(Some(regtest_address("second federation")));
    backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    change_on_its_way(&saving, &backend).await;
    backend
        .set_active_federation(saving.harness.second.id())
        .await
        .unwrap();
    wait_online(&*backend, true).await;
    assert_eq!(
        needs_of(&backend.prepare_refill().await.unwrap()),
        sat(100_000 + DEPOSIT_FEE)
    );
    backend.shutdown().await.unwrap();
}

/// A stalled send blocks a refill until it is replaced: sending from Saving
/// again lifts the block, as the sentence says, and the refill still
/// conflicts with every stalled payment left.
#[tokio::test]
async fn sending_again_lifts_the_block_of_a_stalled_send() {
    let saving = SavingHarness::new();
    // Above what the replacement's change brings on its way.
    let backend = saving.refillable(1_000_000).await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 50_000, 1).await;
    saving.fund(&backend, 5_000, 2).await;
    saving.stalled(&backend).await;
    let reply = backend.prepare_refill().await.map(|_| ());
    let Err(FedimixError::NotSetUp(sentence)) = reply else {
        panic!("{reply:?}");
    };
    assert!(
        sentence.contains("after you send from Saving"),
        "{sentence}"
    );
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    saving.synced(&backend).await;
    saving.chain.clear_broadcast_script();
    assert_eq!(
        send_checked(&saving, &backend, preview).await,
        OperationState::Done
    );
    saving.fund(&backend, 1_200_000, 3).await;
    saving.synced(&backend).await;
    // The replaced send's coins are the replacement's, so the refill spends
    // none of them: only one of the two sends can confirm, and the refill
    // pays no one else.
    let replaced = stalled_txs(&saving);
    let refill = backend.prepare_refill().await.unwrap();
    assert_eq!(
        saving.refill(&backend, refill).await.1,
        OperationState::Done
    );
    let tx = saving.chain.broadcasts().last().unwrap().clone();
    for old in &replaced {
        assert!(
            tx.input.iter().all(|i| old
                .input
                .iter()
                .all(|o| o.previous_output != i.previous_output)),
            "the refill took a coin of the replaced send"
        );
    }
    backend.shutdown().await.unwrap();
}

/// Confirm checks again what the refill must deliver: a review whose
/// overshoot no longer holds expires.
#[tokio::test]
async fn a_changed_gap_expires_the_refill_preview() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_refill().await.unwrap();
    saving.harness.first.set_balance(sat(5_000));
    wait_balance(&*backend, sat(5_000)).await;
    assert_eq!(
        backend.confirm_refill(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
    assert!(saving.chain.broadcasts().is_empty());
    backend.prepare_refill().await.unwrap();
    backend.shutdown().await.unwrap();
}

const NOT_CREDITED: &str = "Your last refill has not reached Spending yet. Refill again once Spending shows it. Until then Fedimix cannot tell what Spending holds, so it refills no federation, this one or another.";

/// Puts `tx` 6 blocks deep and waits for its row to say so.
async fn six_deep(saving: &SavingHarness, backend: &Real, tx: &Transaction) {
    saving.chain.push_sync(confirmed(tx.clone(), 9));
    saving.chain.push_sync(SyncResponse {
        chain_update: Some(chain_to(14)),
        ..Default::default()
    });
    wait_until("6 blocks deep", || async {
        sync_saving_now(backend);
        state_of(saving, tx) == "confirmed"
    })
    .await;
}

/// The federation credits `gross` sat of `txid` to Spending, as each
/// generation reports it, and the balance shows it.
async fn credit(
    saving: &SavingHarness,
    backend: &Real,
    v1: bool,
    watch: &str,
    txid: &str,
    gross: Amount,
) {
    let client = &saving.harness.first;
    let fee = sat(DEPOSIT_FEE);
    let net = gross.checked_sub(fee).unwrap();
    if v1 {
        client.push_update(
            watch,
            WatchUpdate::Settled {
                amount: net,
                fee,
                txid: Some(txid.to_string()),
            },
        );
    } else {
        client.push_deposit(DepositEvent {
            cursor: 2,
            id: format!("{txid}:0"),
            amount: net,
            fee,
            txid: Some(txid.to_string()),
            confirmed: true,
        });
    }
    wait_until("the credited row", || async {
        rows(&**backend).await.iter().any(|r| {
            r.kind == HistoryKind::Deposit
                && r.txid.as_deref() == Some(txid)
                && r.status == HistoryStatus::Complete
        })
    })
    .await;
    let balance = backend.spending_state().await.unwrap().balance;
    let balance = balance.checked_add(net).unwrap();
    client.set_balance(balance);
    wait_balance(&**backend, balance).await;
}

/// RR13: a refill 6 deep that the federation has not credited is still on
/// its way. On v2 the federation sees it only past 6 deep, so no deposit row
/// names it yet; on v1 a confirming row does.
async fn a_refill_counts_until_spending_credits_it(v1: bool) {
    let saving = SavingHarness::new();
    saving.harness.first.set_deposit_watches(v1);
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    saving.fund(&backend, 80_000, 2).await;
    let first = saving.refilled(&backend).await;
    let txid = first.compute_txid().to_string();
    let gross = sat(first.output[0].value.to_sat());
    let watch = saving.harness.first.last_deposit().unwrap();
    if v1 {
        saving.harness.first.push_update(
            &watch,
            WatchUpdate::Confirming {
                confirmations: 0,
                required: 11,
                amount: gross,
                txid: Some(txid.clone()),
            },
        );
    }
    six_deep(&saving, &backend, &first).await;
    assert_eq!(
        backend.prepare_refill().await.map(|_| ()),
        Err(FedimixError::NotSetUp(NOT_CREDITED.into()))
    );
    credit(&saving, &backend, v1, &watch, &txid, gross).await;
    // Credited and 6 deep: the block lifts, and the money counts once, in
    // the balance.
    backend
        .set_settings(Settings {
            spending_target: sat(200_000),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    let arrived = gross.checked_sub(sat(DEPOSIT_FEE)).unwrap();
    let needs = sat(200_000 + DEPOSIT_FEE).checked_sub(arrived).unwrap();
    assert_eq!(needs_of(&backend.prepare_refill().await.unwrap()), needs);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_v1_refill_counts_until_spending_credits_it() {
    a_refill_counts_until_spending_credits_it(true).await;
}

#[tokio::test]
async fn a_v2_refill_counts_until_spending_credits_it() {
    a_refill_counts_until_spending_credits_it(false).await;
}

/// Change 6 deep and not yet credited is still on its way, and counts once
/// after it is credited.
async fn change_counts_until_spending_credits_it(v1: bool) {
    let saving = SavingHarness::new();
    saving.harness.first.set_deposit_watches(v1);
    let backend = saving.refillable(100_000).await;
    let (change, watch, txid) = change_on_its_way(&saving, &backend).await;
    let send = saving
        .chain
        .broadcasts()
        .into_iter()
        .find(|tx| tx.compute_txid().to_string() == txid)
        .unwrap();
    six_deep(&saving, &backend, &send).await;
    assert_eq!(
        needs_of(&backend.prepare_refill().await.unwrap()),
        needs_beside(change)
    );
    credit(&saving, &backend, v1, &watch, &txid, change).await;
    assert_eq!(
        needs_of(&backend.prepare_refill().await.unwrap()),
        needs_beside(change)
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn v1_change_counts_until_spending_credits_it() {
    change_counts_until_spending_credits_it(true).await;
}

#[tokio::test]
async fn v2_change_counts_until_spending_credits_it() {
    change_counts_until_spending_credits_it(false).await;
}

/// What a refill must deliver is checked again after the device signs: a
/// gap that moved while it signed stops the refill before it is stored.
#[tokio::test]
async fn a_gap_that_moves_while_signing_stops_the_refill() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let signer = Arc::new(SlowSigner {
        inner: SoftSigner::from_seed(&SEED, bitcoin::Network::Regtest),
        held: watch::Sender::new(true),
    });
    saving.plug_in(signer.clone());
    let preview = backend.prepare_refill().await.unwrap();
    let events = backend.subscribe();
    let id = backend.confirm_refill(preview.id).await.unwrap();
    wait_state(&backend, &id, OperationState::Signing).await;
    saving.harness.first.set_balance(sat(5_000));
    wait_balance(&*backend, sat(5_000)).await;
    signer.held.send_replace(false);
    let (states, _) = run_of_kind(&events, &id, OperationKind::Refill).await;
    assert_eq!(
        states.last(),
        Some(&OperationState::Failed {
            error: FedimixError::PreviewExpired
        })
    );
    assert!(saving.chain.broadcasts().is_empty());
    assert!(signed_states(&saving).is_empty());
    backend.shutdown().await.unwrap();
}

// ---- The software Saving wallet ----

const SOFTWARE: &str = fedimix_core::SOFTWARE_DEVICE_ID;

fn software_entry() -> fedimix_core::DeviceInfo {
    fedimix_core::DeviceInfo {
        id: SOFTWARE.into(),
        name: "This computer".into(),
        kind: DeviceKind::Software,
        connected: true,
    }
}

/// Checks each input's P2WPKH signature against the coin it spends.
fn assert_signed(tx: &Transaction, coins: &[fedimix_core::Coin]) {
    use bitcoin::secp256k1::{Message, Secp256k1, ecdsa::Signature};
    use bitcoin::sighash::{EcdsaSighashType, SighashCache};
    let secp = Secp256k1::verification_only();
    let mut cache = SighashCache::new(tx);
    for (n, input) in tx.input.iter().enumerate() {
        let coin = coins
            .iter()
            .find(|c| c.outpoint == input.previous_output.to_string())
            .expect("the input is one of ours");
        let witness: Vec<_> = input.witness.iter().collect();
        assert_eq!(witness.len(), 2, "a signature and a key");
        let key = bitcoin::PublicKey::from_slice(witness[1]).unwrap();
        let (sig, flag) = witness[0].split_at(witness[0].len() - 1);
        assert_eq!(flag, [EcdsaSighashType::All as u8]);
        let hash = cache
            .p2wpkh_signature_hash(
                n,
                &script_of(&coin.address),
                bitcoin::Amount::from_sat(coin.amount.as_sat()),
                EcdsaSighashType::All,
            )
            .unwrap();
        let sig = Signature::from_der(sig).unwrap();
        secp.verify_ecdsa(&Message::from(hash), &sig, &key.inner)
            .expect("a valid signature");
    }
}

#[tokio::test]
async fn software_wallet_sets_up_and_sends() {
    let saving = SavingHarness::new();
    let backend = saving.joined_with(SOFTWARE).await;
    let found = backend.detect_devices().await.unwrap().devices;
    assert_eq!(found.last(), Some(&software_entry()), "after the devices");
    let state = backend.saving_state().await.unwrap();
    assert_eq!(state.device, Some(software_entry()));
    assert_eq!(saving.devices.connects(), 0, "no hardware was opened");

    saving.fund(&backend, 100_000, 0).await;
    saving.fund(&backend, 90_000, 1).await;
    let coins = backend.saving_state().await.unwrap().coins;
    let client = &saving.harness.first;
    let address = regtest_address("software change");
    client.set_deposit_address(Some(address.clone()));
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    assert_eq!(preview.federation, Some(client.id()));
    let ChangeDestination::Spending { amount: change } = preview.change else {
        panic!("{:?}", preview.change);
    };
    let (_, state) = saving.send(&backend, preview).await;
    assert_eq!(state, OperationState::Done);
    let tx = saving.chain.broadcasts()[0].clone();
    assert_eq!(paid_to(&tx, &payee()), Some(30_000));
    assert_eq!(paid_to(&tx, &address), Some(change.as_sat()));
    assert_signed(&tx, &coins);
    let rows = saving_rows(&backend).await;
    let row = rows.iter().find(|r| r.id == tx.compute_txid().to_string());
    assert_eq!(
        row.unwrap().kind,
        HistoryKind::Sent {
            change: Some(fedimix_core::SentChange {
                amount: change,
                to_spending: true
            })
        }
    );
    assert_eq!(saving.devices.connects(), 0);
    backend.shutdown().await.unwrap();

    // The words stay in the seed file: no key and no phrase in the database.
    let words = std::fs::read_to_string(saving.harness.dir.path().join("seed")).unwrap();
    for name in ["fedimix.db", "fedimix.db-wal"] {
        let bytes = std::fs::read(saving.harness.dir.path().join(name)).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("prv"), "{name} holds a private key");
        assert!(!text.contains(words.trim()), "{name} holds the words");
    }
}

#[tokio::test]
async fn software_send_needs_no_awaiting_device() {
    let saving = SavingHarness::new();
    let backend = saving.joined_with(SOFTWARE).await;
    saving.fund(&backend, 100_000, 0).await;
    saving.devices.remove(DEVICE);
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    assert!(
        saving.chain.broadcasts().is_empty(),
        "a preview signs nothing"
    );
    let events = backend.subscribe();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(
        states,
        [
            OperationState::Signing,
            OperationState::Broadcasting,
            OperationState::Done
        ]
    );
    assert_eq!(saving.chain.broadcasts().len(), 1);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn software_wallet_is_restored_by_its_words() {
    let first = SavingHarness::new();
    let backend = first.ready_with(SOFTWARE).await;
    let address = backend.receive(on_chain()).await.unwrap().payload;
    backend.shutdown().await.unwrap();
    drop(backend);

    // The manual restore: a new folder holding only the words.
    let words = std::fs::read_to_string(first.harness.dir.path().join("seed")).unwrap();
    let second = SavingHarness::new();
    let seed = second.harness.dir.path().join("seed");
    std::fs::write(&seed, &words).unwrap();
    std::fs::set_permissions(&seed, std::fs::Permissions::from_mode(0o600)).unwrap();
    let restored = second.ready_with(SOFTWARE).await;
    assert_eq!(
        restored.receive(on_chain()).await.unwrap().payload,
        address,
        "the same account"
    );
    restored.shutdown().await.unwrap();
    drop(restored);

    // The same stored account: kind, name, fingerprint, xpub and path.
    let account = |saving: &SavingHarness| {
        raw_store(&saving.harness)
            .query_row(
                "SELECT kind, name, fingerprint, xpub, path FROM saving_device WHERE id = 1",
                [],
                |r| {
                    (0..5)
                        .map(|n| r.get::<_, String>(n))
                        .collect::<rusqlite::Result<Vec<_>>>()
                },
            )
            .unwrap()
    };
    let stored = account(&first);
    assert_eq!(stored, account(&second));
    assert_eq!(stored[1], "This computer");
    assert_eq!(stored[2].len(), 8, "a fingerprint: {stored:?}");
    assert!(stored[3].starts_with("tpub"), "{stored:?}");
    assert_eq!(stored[4], "m/84'/1'/0'");

    // After a restart the software wallet is there without a poll.
    let backend = second.open().await.unwrap();
    assert_eq!(
        backend.saving_state().await.unwrap().device,
        Some(software_entry())
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn software_wallet_works_through_a_proxy() {
    let saving = SavingHarness::new();
    let backend = saving.with_server().await;
    backend
        .set_settings(Settings {
            socks5_proxy: Some("127.0.0.1:9050".into()),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    let found = backend.detect_devices().await.unwrap().devices;
    assert_eq!(found.last(), Some(&software_entry()));
    saving.chain.push_full_scan(first_scan(1));
    backend.setup_saving(SOFTWARE.into()).await.unwrap();
    wait_chain(&*backend, true).await;
    assert_eq!(
        backend.saving_state().await.unwrap().device,
        Some(software_entry())
    );
    backend.shutdown().await.unwrap();
}

const SCAN_FAILED: &str = "Fedimix could not look for USB devices.";

#[tokio::test]
async fn a_failed_scan_is_shown_beside_the_software_wallet() {
    let saving = SavingHarness::new();
    let backend = saving.with_server().await;
    saving
        .devices
        .fail_listing(Some(FedimixError::Internal(SCAN_FAILED.into())));
    assert_eq!(
        backend.detect_devices().await.unwrap(),
        fedimix_core::DeviceList {
            devices: vec![software_entry()],
            problem: Some(SCAN_FAILED.into()),
        }
    );
    saving.devices.fail_listing(None);
    let listing = backend.detect_devices().await.unwrap();
    assert_eq!(listing.problem, None);
    assert_eq!(listing.devices.len(), 2);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_scan_fails_a_hardware_send_at_once() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    let coin = saving.fund(&backend, 100_000, 0).await;
    let preview = backend.prepare_send(saving_sat(30_000)).await.unwrap();
    saving
        .devices
        .fail_listing(Some(FedimixError::Internal(SCAN_FAILED.into())));
    let events = backend.subscribe();
    let id = backend.confirm_send(preview.id).await.unwrap();
    let (states, _) = run_of(&events, &id).await;
    assert_eq!(
        states,
        [
            OperationState::AwaitingDevice,
            OperationState::Failed {
                error: FedimixError::Internal(SCAN_FAILED.into())
            }
        ]
    );
    assert!(saving.chain.broadcasts().is_empty());
    assert!(coin_at(&backend, &coin).await.is_some());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn software_wallet_refills() {
    let saving = SavingHarness::new();
    let backend = saving.joined_with(SOFTWARE).await;
    backend
        .set_settings(Settings {
            spending_target: sat(25_000),
            ..chain_settings(CHAIN_URL)
        })
        .await
        .unwrap();
    saving.fund(&backend, 50_000, 0).await;
    let coins = backend.saving_state().await.unwrap().coins;
    let preview = backend.prepare_refill().await.unwrap();
    assert_eq!(preview.device_name, "This computer");
    let events = backend.subscribe();
    let id = backend.confirm_refill(preview.id).await.unwrap();
    let (states, _) = run_of_kind(&events, &id, OperationKind::Refill).await;
    assert_eq!(
        states,
        [
            OperationState::Signing,
            OperationState::Broadcasting,
            OperationState::Done
        ]
    );
    let tx = saving.chain.broadcasts().last().unwrap().clone();
    assert_eq!(tx.output.len(), 1, "no change");
    assert_signed(&tx, &coins);
    backend.shutdown().await.unwrap();
}

/// Spending credits a payment of Saving's: `tx`, to the federation's deposit
/// `watch`, `gross` sat. Saving's next sync, whenever it comes, shows `tx` in
/// a block. Then its row in Saving reaches `Complete` and Saving tells the
/// front end, with no restart and no sync asked for by the test.
async fn credited_payment_finishes_its_row(
    saving: &SavingHarness,
    backend: &Real,
    tx: &Transaction,
    watch: &str,
    gross: Amount,
) {
    let txid = tx.compute_txid().to_string();
    // The sync that the broadcast asked for has run and found it pending,
    // and so has any a wake left over from `synced` starts: Saving is idle.
    saving.synced(backend).await;
    let scans = saving.chain.scans_made();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        saving.chain.scans_made() <= scans + 1,
        "Saving keeps syncing"
    );
    let row = |rows: Vec<HistoryItem>| rows.into_iter().find(|r| r.txid.as_ref() == Some(&txid));
    assert_eq!(
        row(saving_rows(backend).await).map(|r| r.status),
        Some(HistoryStatus::Pending)
    );
    saving.chain.push_sync(confirmed(tx.clone(), 9));
    let events = backend.subscribe();
    credit(saving, backend, false, watch, &txid, gross).await;
    wait_until("Saving's row to finish", || async {
        row(saving_rows(backend).await).is_some_and(|r| r.status == HistoryStatus::Complete)
    })
    .await;
    within("Saving's event", async {
        while !matches!(
            events.next().await,
            Some(Event::SavingChanged)
                | Some(Event::HistoryItemUpdated {
                    wallet: WalletKind::Saving,
                    ..
                })
        ) {}
    })
    .await;
}

/// A refill's row in Saving is finished once Spending credits it, without a
/// restart: the credit is news for Saving too.
#[tokio::test]
async fn a_credited_refill_finishes_its_saving_row() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 100_000, 0).await;
    let tx = saving.refilled(&backend).await;
    let watch = saving.harness.first.last_deposit().unwrap();
    let gross = sat(tx.output[0].value.to_sat());
    credited_payment_finishes_its_row(&saving, &backend, &tx, &watch, gross).await;
    backend.shutdown().await.unwrap();
}

/// The same for a Saving send whose change Spending credits.
#[tokio::test]
async fn a_credited_send_finishes_its_saving_row() {
    let saving = SavingHarness::new();
    let backend = saving.refillable(100_000).await;
    let (change, watch, txid) = change_on_its_way(&saving, &backend).await;
    let tx = saving
        .chain
        .broadcasts()
        .into_iter()
        .find(|tx| tx.compute_txid().to_string() == txid)
        .unwrap();
    credited_payment_finishes_its_row(&saving, &backend, &tx, &watch, change).await;
    backend.shutdown().await.unwrap();
}

// Session sync and anonymity estimates.

/// 0.001 BTC, the smallest denomination and the federation estimate's.
const SMALLEST: Amount = Amount { msat: 100_000_000 };

fn chain_deposit(sat: u64, outpoint: &str) -> SessionDeposit {
    SessionDeposit {
        source: DepositSource::OnChain,
        amount: Amount::from_sat(sat),
        reference: Some(outpoint.to_string()),
    }
}

fn lightning_deposit(sat: u64) -> SessionDeposit {
    SessionDeposit {
        source: DepositSource::Lightning,
        amount: Amount::from_sat(sat),
        reference: None,
    }
}

/// An outpoint unique to `n`.
fn outpoint(n: u64) -> String {
    format!("{n:064x}:0")
}

/// The next session the sync of the first federation would fetch.
fn anon_cursor(harness: &Harness) -> Option<u64> {
    use rusqlite::OptionalExtension as _;
    raw_store(harness)
        .query_row(
            "SELECT value FROM cursors WHERE name = ?1",
            [format!("anon:{FIRST_FEDERATION}")],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .unwrap()
        .map(|value| u64::try_from(value).unwrap())
}

/// The sessions holding stored deposits of the first federation.
fn stored_sessions(harness: &Harness) -> Vec<u64> {
    let conn = raw_store(harness);
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT session FROM anon_deposits WHERE federation_id = ?1
             ORDER BY session",
        )
        .unwrap();
    stmt.query_map([FIRST_FEDERATION], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|session| u64::try_from(session.unwrap()).unwrap())
        .collect()
}

async fn wait_synced_to(harness: &Harness, cursor: u64) {
    wait_until("the session sync", || async {
        anon_cursor(harness) == Some(cursor)
    })
    .await;
}

async fn federation_estimate(backend: &dyn Backend) -> u32 {
    backend.federations().await.unwrap()[0].anonymity_estimate
}

async fn wait_federation_estimate(backend: &dyn Backend, estimate: u32) {
    wait_until("the federation estimate", || async {
        federation_estimate(backend).await == estimate
    })
    .await;
}

#[tokio::test]
async fn sync_starts_at_the_join() {
    let harness = Harness::new();
    for n in 0..3 {
        harness
            .first
            .push_session(vec![chain_deposit(200_000, &outpoint(n))]);
    }
    let backend = harness.joined().await;
    let events = backend.subscribe();
    for n in 3..5 {
        harness
            .first
            .push_session(vec![chain_deposit(200_000, &outpoint(n))]);
    }
    wait_synced_to(&harness, 5).await;
    wait_event(&events, &Event::QueueChanged).await;
    assert_eq!(stored_sessions(&harness), [3, 4]);
    for session in 0..3 {
        assert_eq!(
            harness.first.session_fetches(session),
            0,
            "session {session}"
        );
    }
    wait_federation_estimate(&*backend, 2).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn sync_resumes_from_its_cursor_after_restart() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    for n in 0..2 {
        harness
            .first
            .push_session(vec![chain_deposit(200_000, &outpoint(n))]);
    }
    wait_synced_to(&harness, 2).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    for n in 2..4 {
        harness
            .first
            .push_session(vec![chain_deposit(200_000, &outpoint(n))]);
    }
    let backend = harness.open().await.unwrap();
    wait_online(&*backend, true).await;
    wait_synced_to(&harness, 4).await;
    for session in 0..4 {
        assert_eq!(
            harness.first.session_fetches(session),
            1,
            "session {session}"
        );
    }
    assert_eq!(stored_sessions(&harness), [0, 1, 2, 3]);
    wait_federation_estimate(&*backend, 4).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn own_deposits_are_left_out_of_the_count() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    harness.first.push_session(Vec::new());
    wait_synced_to(&harness, 1).await;

    // A deposit known by its txid, as on v1.
    let info = backend.receive(address_request()).await.unwrap();
    let by_txid = watch_id(&info.payload).to_string();
    let own_txid = format!("{:064x}", 1);
    harness.first.push_update(
        &by_txid,
        WatchUpdate::Settled {
            amount: Amount::from_sat(199_000),
            fee: Amount::from_sat(1_000),
            txid: Some(own_txid.clone()),
        },
    );
    // A deposit known by its outpoint, as on v2.
    let own_outpoint = format!("{:064x}:1", 2);
    harness.first.push_deposit(DepositEvent {
        cursor: 1,
        id: own_outpoint.clone(),
        amount: Amount::from_sat(199_000),
        fee: Amount::from_sat(1_000),
        txid: Some(format!("{:064x}", 2)),
        confirmed: true,
    });
    // A Lightning receive, which is kept with its session.
    let info = backend.receive(invoice_request(200_000)).await.unwrap();
    let by_lightning = watch_id(&info.payload).to_string();
    harness.first.push_update(&by_lightning, settled(200_000));
    for id in [&by_txid, &own_outpoint, &by_lightning] {
        wait_status(&*backend, id, HistoryStatus::Complete).await;
    }
    let row = rows(&*backend)
        .await
        .into_iter()
        .find(|row| row.id == by_lightning)
        .unwrap();
    assert_eq!(row.session, Some(1));

    harness.first.push_session(vec![
        chain_deposit(200_000, &format!("{own_txid}:0")),
        chain_deposit(200_000, &own_outpoint),
        chain_deposit(200_000, &outpoint(3)),
        lightning_deposit(200_000),
        lightning_deposit(200_000),
    ]);
    wait_synced_to(&harness, 2).await;
    // Someone else's deposit and one of the two Lightning receives.
    wait_federation_estimate(&*backend, 2).await;
    let estimate = |sat| item_estimate(&backend, FIRST_FEDERATION, 0, Amount::from_sat(sat));
    assert_eq!(estimate(200_000).unwrap(), 2);
    assert_eq!(estimate(500_000).unwrap(), 0);
    // The item's own session never counts.
    assert_eq!(
        item_estimate(&backend, FIRST_FEDERATION, 1, SMALLEST).unwrap(),
        0
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn federation_estimate_counts_the_recent_window() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    // 600 sessions: the oldest 120 each hold 0.01 BTC, the newest 480 one
    // 0.001 BTC deposit every four sessions and 0.0005 BTC in between.
    for n in 0..600 {
        let sat = match n {
            n if n < 120 => 1_000_000,
            n if n % 4 == 0 => 100_000,
            _ => 50_000,
        };
        harness
            .first
            .push_session(vec![chain_deposit(sat, &outpoint(n))]);
    }
    wait_synced_to(&harness, 600).await;
    wait_federation_estimate(&*backend, 120).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_undecodable_session_stalls_counting_but_not_exits() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    for n in 0..3 {
        harness
            .first
            .push_session(vec![chain_deposit(100_000, &outpoint(n))]);
    }
    wait_synced_to(&harness, 3).await;
    wait_federation_estimate(&*backend, 3).await;

    harness.first.fail_session(
        3,
        FedimixError::Internal("This session does not decode.".into()),
    );
    for n in 3..6 {
        harness
            .first
            .push_session(vec![chain_deposit(100_000, &outpoint(n))]);
    }
    // Tried again, never skipped.
    wait_until("a retry of the failed session", || async {
        harness.first.session_fetches(3) >= 2
    })
    .await;
    assert_eq!(anon_cursor(&harness), Some(3));
    assert_eq!(stored_sessions(&harness), [0, 1, 2]);
    for session in 4..6 {
        assert_eq!(
            harness.first.session_fetches(session),
            0,
            "session {session}"
        );
    }
    assert_eq!(federation_estimate(&*backend).await, 3);
    // An item's estimate still answers, with what was counted.
    assert_eq!(
        item_estimate(&backend, FIRST_FEDERATION, 0, SMALLEST).unwrap(),
        2
    );

    harness.first.clear_session_failure(3);
    wait_synced_to(&harness, 6).await;
    assert_eq!(stored_sessions(&harness), [0, 1, 2, 3, 4, 5]);
    wait_federation_estimate(&*backend, 6).await;
    assert_eq!(
        item_estimate(&backend, FIRST_FEDERATION, 0, SMALLEST).unwrap(),
        5
    );
    backend.shutdown().await.unwrap();
}

// The denomination queue (Phase 4 Task 5).

/// `n` thousandths of a bitcoin.
fn mbtc(n: u64) -> Amount {
    Amount::from_sat(n * 100_000)
}

async fn queue_of(backend: &dyn Backend) -> Vec<Amount> {
    let queue = backend.queue_state().await.unwrap();
    queue.items.iter().map(|item| item.denomination).collect()
}

async fn queue_ids(backend: &dyn Backend) -> Vec<String> {
    let queue = backend.queue_state().await.unwrap();
    queue.items.into_iter().map(|item| item.id).collect()
}

async fn wait_queue(backend: &dyn Backend, expected: &[Amount]) {
    wait_until("the queue", || async {
        queue_of(backend).await == expected
    })
    .await;
}

/// The id, origin session and origin time of each item of the first
/// federation, oldest first.
fn origins(harness: &Harness) -> Vec<(String, u64, u64)> {
    let conn = raw_store(harness);
    let mut stmt = conn
        .prepare(
            "SELECT id, origin_session, origin_at FROM exit_items
             WHERE federation_id = ?1 ORDER BY seq",
        )
        .unwrap();
    stmt.query_map([FIRST_FEDERATION], |r| {
        Ok((
            r.get::<_, String>(0)?,
            u64::try_from(r.get::<_, i64>(1)?).unwrap(),
            u64::try_from(r.get::<_, i64>(2)?).unwrap(),
        ))
    })
    .unwrap()
    .map(|row| row.unwrap())
    .collect()
}

/// 0.47 BTC at the default target of 0.05 BTC.
async fn queued_470(harness: &Harness) -> Arc<fedimix_core::RealBackend> {
    harness.first.set_balance(mbtc(470));
    let backend = harness.joined().await;
    wait_queue(&*backend, &[mbtc(200), mbtc(200), mbtc(20)]).await;
    backend
}

#[tokio::test]
async fn deposits_above_target_are_queued_in_denominations() {
    let harness = Harness::new();
    let backend = queued_470(&harness).await;
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.balance, mbtc(470));
    assert_eq!(state.moving, mbtc(420));
    assert_eq!(state.for_spending, mbtc(50));
    let queue = backend.queue_state().await.unwrap();
    assert_eq!(queue.total, mbtc(420));
    assert!(
        queue
            .items
            .iter()
            .all(|item| item.status == fedimix_core::QueueItemStatus::Queued),
        "{:?}",
        queue.items
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_remainder_below_the_smallest_stays_in_spending() {
    let harness = Harness::new();
    harness.first.set_balance(sat(5_150_000));
    let backend = harness.joined().await;
    wait_queue(&*backend, &[mbtc(1)]).await;
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.moving, mbtc(1));
    assert_eq!(state.for_spending, sat(5_050_000));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn refill_overshoot_rejoins_the_queue() {
    let saving = SavingHarness::new();
    let client = &saving.harness.first;
    client.set_session_count(Ok(42));
    let backend = saving.refillable(30_000).await;
    saving.fund(&backend, 300_000, 0).await;
    let tx = saving.refilled(&backend).await;
    let txid = tx.compute_txid().to_string();
    let gross = sat(tx.output[0].value.to_sat());
    let net = gross.checked_sub(sat(DEPOSIT_FEE)).unwrap();
    let watch = client.last_deposit().unwrap();
    client.push_update(
        &watch,
        WatchUpdate::Settled {
            amount: net,
            fee: sat(DEPOSIT_FEE),
            txid: Some(txid.clone()),
        },
    );
    wait_status(&*backend, &watch, HistoryStatus::Complete).await;
    let row = row_of(&rows(&*backend).await, &watch);
    assert_eq!(row.session, Some(42));
    // Later sessions have passed when Spending shows the money.
    client.set_session_count(Ok(50));
    client.set_balance(net);
    // About 0.0027 BTC above the target of 30k sat.
    wait_queue(&*backend, &[mbtc(2)]).await;
    let stored = origins(&saving.harness);
    assert_eq!(stored.len(), 1);
    assert_eq!((stored[0].1, stored[0].2), (42, row.timestamp));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_send_takes_from_the_queue_newest_first() {
    let harness = Harness::new();
    let backend = queued_470(&harness).await;
    let before = queue_ids(&*backend).await;
    let preview = backend
        .prepare_send(to(ADDRESS, SendAmount::Exact(mbtc(30))))
        .await
        .unwrap();
    backend.confirm_send(preview.id).await.unwrap();
    // 0.47 - 0.03 - 300 sat leaves room for 0.389997 BTC above the target:
    // the 0.02 and the newer 0.2 give way, and what they freed is split again.
    let expected = [
        mbtc(200),
        mbtc(100),
        mbtc(50),
        mbtc(20),
        mbtc(10),
        mbtc(5),
        mbtc(2),
        mbtc(2),
    ];
    wait_queue(&*backend, &expected).await;
    let after = queue_ids(&*backend).await;
    assert_eq!(after[0], before[0], "the oldest item stays");
    assert!(!after.contains(&before[1]) && !after.contains(&before[2]));
    let balance = sat(47_000_000 - 3_000_000 - 300);
    wait_balance(&*backend, balance).await;
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.moving, mbtc(389));
    assert_eq!(state.for_spending, balance.checked_sub(mbtc(389)).unwrap());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_review_says_when_a_send_takes_from_the_queue() {
    let harness = Harness::new();
    // 0.0009 BTC above the queue and the target: a small send fits there.
    harness.first.set_balance(sat(47_090_000));
    let backend = harness.joined().await;
    wait_queue(&*backend, &[mbtc(200), mbtc(200), mbtc(20)]).await;
    let small = backend
        .prepare_send(to(ADDRESS, SendAmount::Exact(sat(500))))
        .await
        .unwrap();
    assert_eq!(small.notes, Vec::<String>::new());

    harness.first.set_balance(mbtc(470));
    wait_balance(&*backend, mbtc(470)).await;
    let preview = backend
        .prepare_send(to(ADDRESS, SendAmount::Exact(mbtc(10))))
        .await
        .unwrap();
    assert_eq!(
        preview.notes,
        ["0.011 BTC of this comes from money moving to Saving.".to_string()]
    );
    // A review changes nothing.
    assert_eq!(queue_of(&*backend).await, [mbtc(200), mbtc(200), mbtc(20)]);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn raising_the_target_shrinks_the_queue() {
    let harness = Harness::new();
    let backend = queued_470(&harness).await;
    let first = queue_ids(&*backend).await.remove(0);
    let settings = backend.settings().await.unwrap();
    backend
        .set_settings(Settings {
            spending_target: mbtc(100),
            ..settings
        })
        .await
        .unwrap();
    wait_queue(&*backend, &[mbtc(200), mbtc(100), mbtc(50), mbtc(20)]).await;
    assert_eq!(queue_ids(&*backend).await[0], first);
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.moving, mbtc(370));
    assert_eq!(state.for_spending, mbtc(100));

    // A lower target books what now lies above it, with no balance change.
    let settings = backend.settings().await.unwrap();
    backend
        .set_settings(Settings {
            spending_target: mbtc(50),
            ..settings
        })
        .await
        .unwrap();
    wait_queue(
        &*backend,
        &[mbtc(200), mbtc(100), mbtc(50), mbtc(20), mbtc(50)],
    )
    .await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn queue_follows_the_active_federation() {
    let harness = Harness::new();
    harness.second.set_balance(mbtc(60));
    let backend = queued_470(&harness).await;
    let second = backend.join_federation(SECOND_INVITE.into()).await.unwrap();
    // The first federation stays active.
    assert_eq!(queue_of(&*backend).await, [mbtc(200), mbtc(200), mbtc(20)]);
    backend.set_active_federation(second.id).await.unwrap();
    wait_queue(&*backend, &[mbtc(10)]).await;
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.moving, mbtc(10));
    assert_eq!(state.for_spending, mbtc(50));
    backend.shutdown().await.unwrap();
}

async fn assert_waiting(backend: &dyn Backend, waiting: bool) {
    let queue = backend.queue_state().await.unwrap();
    assert_eq!(queue.waiting_for_saving, waiting);
    assert_eq!(queue.items.len(), 3);
    assert!(
        queue
            .items
            .iter()
            .all(|item| item.status == fedimix_core::QueueItemStatus::Queued),
        "{:?}",
        queue.items
    );
}

/// Review Focus 4: Saving not set up, scanning, or unable to open.
#[tokio::test]
async fn without_saving_the_queue_waits() {
    let saving = SavingHarness::new();
    saving.harness.first.set_balance(mbtc(470));
    let backend = saving.with_server().await;
    backend.join_federation(INVITE.into()).await.unwrap();
    wait_queue(&*backend, &[mbtc(200), mbtc(200), mbtc(20)]).await;
    assert_waiting(&*backend, true).await;

    saving.chain.set_stalled(true);
    saving.chain.push_full_scan(first_scan(1));
    backend.setup_saving(DEVICE.into()).await.unwrap();
    assert!(backend.saving_state().await.unwrap().scanning);
    assert_waiting(&*backend, true).await;
    saving.chain.set_stalled(false);
    wait_chain(&*backend, true).await;
    assert_waiting(&*backend, false).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    raw_store(&saving.harness)
        .execute(
            "UPDATE saving_device SET xpub = 'not a key' WHERE id = 1",
            [],
        )
        .unwrap();
    let backend = saving.open().await.unwrap();
    assert!(!backend.saving_state().await.unwrap().wallet_open);
    assert_waiting(&*backend, true).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn in_flight_counts_exits_until_they_confirm() {
    let saving = SavingHarness::new();
    saving.harness.first.set_balance(mbtc(470));
    let backend = saving.joined().await;
    wait_queue(&*backend, &[mbtc(200), mbtc(200), mbtc(20)]).await;
    assert_eq!(
        backend.saving_state().await.unwrap().in_flight,
        Amount::ZERO
    );
    let raw = raw_store(&saving.harness);
    raw.execute(
        "INSERT INTO exit_items (id, federation_id, denomination_msat, origin_session,
            origin_at, created_at, state, intent, address)
         VALUES ('leaving', ?1, 1000000000, 0, 0, 0, 'withdrawing', 'exit-leaving', 'address')",
        [FIRST_FEDERATION],
    )
    .unwrap();
    assert_eq!(backend.saving_state().await.unwrap().in_flight, mbtc(10));
    let queue = backend.queue_state().await.unwrap();
    let leaving = queue
        .items
        .iter()
        .find(|item| item.id == "leaving")
        .unwrap();
    assert_eq!(
        leaving.status,
        fedimix_core::QueueItemStatus::Withdrawing { txid: None }
    );
    // Only what is still queued is moving: an exit on its way has left.
    assert_eq!(queue.total, mbtc(420));
    assert_eq!(backend.spending_state().await.unwrap().moving, mbtc(420));

    raw.execute(
        "UPDATE exit_items SET state = 'landed' WHERE id = 'leaving'",
        [],
    )
    .unwrap();
    assert_eq!(
        backend.saving_state().await.unwrap().in_flight,
        Amount::ZERO
    );
    assert_eq!(queue_of(&*backend).await, [mbtc(200), mbtc(200), mbtc(20)]);
    backend.shutdown().await.unwrap();
}

/// `in_flight` is exits only now; what cannot be sent still counts coins
/// received and not yet confirmed.
#[tokio::test]
async fn held_back_still_counts_unconfirmed_receives() {
    let saving = SavingHarness::new();
    let backend = saving.joined().await;
    saving.fund(&backend, 100_000, 0).await;
    let info = backend.receive(on_chain()).await.unwrap();
    let tx = payment(&info.payload, 40_000, 7);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    saving.chain.push_sync(in_mempool(tx, now, 2));
    wait_until("the unconfirmed coin held back", || async {
        sync_saving_now(&backend);
        backend.saving_state().await.unwrap().held_back == sat(40_000)
    })
    .await;
    assert_eq!(
        backend.saving_state().await.unwrap().in_flight,
        Amount::ZERO
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn queue_survives_restart() {
    let harness = Harness::new();
    let backend = queued_470(&harness).await;
    let before = origins(&harness);
    let backend = restarted(&harness, backend).await;
    wait_online(&*backend, true).await;
    // One more thousandth: the queue grows by one item and keeps the rest.
    harness.first.set_balance(mbtc(471));
    wait_queue(&*backend, &[mbtc(200), mbtc(200), mbtc(20), mbtc(1)]).await;
    assert_eq!(origins(&harness)[..3], before[..]);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_repeated_outpoint_counts_once() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    let deposit = chain_deposit(200_000, &outpoint(1));
    harness
        .first
        .push_session(vec![deposit.clone(), deposit.clone()]);
    harness.first.push_session(vec![deposit]);
    harness
        .first
        .push_session(vec![chain_deposit(200_000, &outpoint(2))]);
    wait_synced_to(&harness, 3).await;
    wait_federation_estimate(&*backend, 2).await;
    assert_eq!(
        item_estimate(&backend, FIRST_FEDERATION, 0, SMALLEST).unwrap(),
        1
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_pending_lightning_receive_is_ours_at_once() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.receive(invoice_request(200_000)).await.unwrap();
    // Its contract lands before the receive settles, so before it has a session.
    harness
        .first
        .push_session(vec![lightning_deposit(200_000), lightning_deposit(200_000)]);
    wait_synced_to(&harness, 1).await;
    wait_federation_estimate(&*backend, 1).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_receive_with_no_session_spares_windows_that_ended_before_it() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    harness.first.push_session(vec![lightning_deposit(200_000)]);
    wait_synced_to(&harness, 1).await;
    wait_federation_estimate(&*backend, 1).await;

    // Asked for after that session was synced: the window ending there keeps
    // someone else's receive.
    harness.time.store(2_000, Ordering::SeqCst);
    backend.receive(invoice_request(200_000)).await.unwrap();
    assert_eq!(federation_estimate(&*backend).await, 1);
    assert_eq!(
        item_estimate(&backend, FIRST_FEDERATION, 0, SMALLEST).unwrap(),
        0
    );

    // A window ending later may hold its contract, so it is left out there.
    harness.time.store(3_000, Ordering::SeqCst);
    harness.first.push_session(Vec::new());
    wait_synced_to(&harness, 2).await;
    wait_federation_estimate(&*backend, 0).await;
    backend.shutdown().await.unwrap();
}

/// I1: tending the queue never holds up the connection. With the session
/// count hanging, balance changes still arrive, and the queue fills once the
/// count answers.
#[tokio::test]
async fn a_federation_slow_to_count_sessions_delays_no_balance_event() {
    let harness = Harness::new();
    let backend = harness.joined().await;
    backend.shutdown().await.unwrap();
    drop(backend);
    harness.first.set_session_count_held(true);
    harness.first.set_balance(mbtc(470));
    let backend = harness.open().await.unwrap();
    let events = backend.subscribe();
    wait_online(&*backend, true).await;
    wait_balance(&*backend, mbtc(470)).await;
    harness.first.set_balance(mbtc(471));
    wait_event(
        &events,
        &Event::BalanceChanged {
            wallet: WalletKind::Spending,
            balance: mbtc(471),
        },
    )
    .await;
    assert_eq!(queue_of(&*backend).await, Vec::<Amount>::new());
    harness.first.set_session_count_held(false);
    wait_queue(&*backend, &[mbtc(200), mbtc(200), mbtc(20), mbtc(1)]).await;
    backend.shutdown().await.unwrap();
}

/// I3: a send whose row was written but whose fit never ran (a crash between
/// the two) still takes its money from the queue after a restart, while it is
/// unresolved.
#[tokio::test]
async fn a_send_cut_off_before_its_fit_shrinks_the_queue_after_restart() {
    let harness = Harness::new();
    let backend = queued_470(&harness).await;
    halt_sends(&backend, SendHalt::AfterRow);
    let preview = backend
        .prepare_send(to(ADDRESS, SendAmount::Exact(mbtc(30))))
        .await
        .unwrap();
    backend.confirm_send(preview.id).await.unwrap();
    assert_eq!(queue_of(&*backend).await, [mbtc(200), mbtc(200), mbtc(20)]);
    // The lookup that would settle the intent fails, so it stays unresolved.
    harness.first.fail_next_find(FedimixError::Offline);
    let backend = restarted(&harness, backend).await;
    wait_queue(
        &*backend,
        &[
            mbtc(200),
            mbtc(100),
            mbtc(50),
            mbtc(20),
            mbtc(10),
            mbtc(5),
            mbtc(2),
            mbtc(2),
        ],
    )
    .await;
    backend.shutdown().await.unwrap();
}

// Exits to Saving.

const EXIT_FEE: Amount = Amount { msat: 2_000_000 };
const EXIT_NEVER_SENT: &str = "This exit was never sent, so no money left Spending.";
const EXIT_REFUSED: &str = "The federation refused the withdrawal.";

/// The chain server set and no grace period, so an exit can go at once.
fn exit_settings() -> Settings {
    Settings {
        online_grace_secs: 0,
        ..chain_settings(CHAIN_URL)
    }
}

fn exit_txid(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

fn exit_settled(txid: &str) -> WatchUpdate {
    WatchUpdate::Settled {
        amount: mbtc(10),
        fee: EXIT_FEE,
        txid: Some(txid.to_string()),
    }
}

impl SavingHarness {
    /// Saving on "This computer", joined, with `balance` in Spending at the
    /// default target and no grace period, once the queue reads `queue`.
    async fn exiting(&self, balance: Amount, queue: &[Amount]) -> Real {
        self.harness.first.set_balance(balance);
        self.harness.first.set_withdraw_fee(EXIT_FEE);
        let backend = self.joined_with(SOFTWARE).await;
        backend.set_settings(exit_settings()).await.unwrap();
        wait_queue(&*backend, queue).await;
        backend
    }

    /// Shut down and opened again on the same folder, not yet online.
    async fn reopened(&self, backend: Real) -> Real {
        backend.shutdown().await.unwrap();
        drop(backend);
        self.open().await.unwrap()
    }
}

/// The id of the first queued item of `denomination`.
async fn item_of(backend: &dyn Backend, denomination: Amount) -> String {
    let queue = backend.queue_state().await.unwrap();
    let item = queue
        .items
        .into_iter()
        .find(|item| item.denomination == denomination);
    item.expect("an item of that denomination").id
}

async fn status_of(backend: &dyn Backend, item: &str) -> Option<fedimix_core::QueueItemStatus> {
    let queue = backend.queue_state().await.unwrap();
    queue
        .items
        .into_iter()
        .find(|shown| shown.id == item)
        .map(|shown| shown.status)
}

/// Every address an exit of `item` was ever given, oldest first.
fn exit_addresses_of(harness: &Harness, item: &str) -> Vec<String> {
    let conn = raw_store(harness);
    let mut stmt = conn
        .prepare("SELECT address FROM exit_addresses WHERE item_id = ?1 ORDER BY rowid")
        .unwrap();
    stmt.query_map([item], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|address| address.unwrap())
        .collect()
}

/// Previews and confirms an exit of `item`: the preview, and the exit's id.
async fn exit_item(backend: &Real, item: &str) -> (fedimix_core::ExitPreview, String) {
    let preview = backend.prepare_exit(item.into()).await.unwrap();
    let id = backend.exit_now(preview.id.clone()).await.unwrap();
    (preview, id)
}

async fn wait_operation_on_disk(harness: &Harness, id: &str, operation: &str) {
    wait_until("the operation id on disk", || async {
        raw_store(harness)
            .query_row("SELECT operation FROM watches WHERE id = ?1", [id], |row| {
                row.get::<_, Option<String>>(0)
            })
            .unwrap()
            .as_deref()
            == Some(operation)
    })
    .await;
}

#[tokio::test]
async fn exit_now_pays_a_fresh_saving_address() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let given = backend.receive(on_chain()).await.unwrap().payload;
    let item = item_of(&*backend, mbtc(10)).await;
    let preview = backend.prepare_exit(item.clone()).await.unwrap();
    assert_eq!(preview.item_id, item);
    assert_eq!(preview.fee, EXIT_FEE);
    assert_eq!(
        saving.harness.first.withdraw_calls(),
        0,
        "a preview pays nothing"
    );
    // Handed out after the preview: the exit must not take it.
    let after_preview = backend.receive(on_chain()).await.unwrap().payload;
    let id = backend.exit_now(preview.id).await.unwrap();
    wait_until("the withdrawal", || async {
        saving.harness.first.withdrawn().len() == 1
    })
    .await;
    let quote = saving.harness.first.withdrawn().remove(0);
    assert_eq!(quote.amount, mbtc(10), "exactly the denomination");
    assert_eq!(quote.fee, EXIT_FEE);
    assert_ne!(quote.address, given);
    assert_ne!(quote.address, after_preview);
    let next = backend.receive(on_chain()).await.unwrap().payload;
    assert_ne!(
        next, quote.address,
        "the exit's address is never handed out"
    );
    assert_eq!(exit_addresses_of(&saving.harness, &item), [quote.address]);
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Withdrawing { txid: None })
    );
    let row = row_of(&rows(&*backend).await, &id);
    assert_eq!(
        row.kind,
        HistoryKind::ToSaving {
            denomination: mbtc(10)
        }
    );
    assert_eq!(row.amount, mbtc(10));
    assert_eq!(row.fee, Some(EXIT_FEE));
    assert!(!row.incoming);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_exit_reports_as_an_exit() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    let events = backend.subscribe();
    let (_, id) = exit_item(&backend, &item).await;
    wait_event(
        &events,
        &Event::Operation {
            id: id.clone(),
            kind: OperationKind::Exit,
            state: OperationState::Broadcasting,
            history_id: None,
        },
    )
    .await;
    let op = operation_of(&saving.harness.first, &id).await;
    wait_operation_on_disk(&saving.harness, &id, &op).await;
    let running = vec![RunningOperation {
        id: id.clone(),
        kind: OperationKind::Exit,
        state: OperationState::Broadcasting,
    }];
    assert_eq!(backend.operations().await.unwrap(), running);

    let backend = saving.reopened(backend).await;
    assert_eq!(backend.operations().await.unwrap(), running);
    let events = backend.subscribe();
    let txid = exit_txid(1);
    saving.harness.first.push_update(&op, exit_settled(&txid));
    let (states, history_id) = run_of_kind(&events, &id, OperationKind::Exit).await;
    assert_eq!(states, [OperationState::Done]);
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    assert_eq!(row_of(&rows(&*backend).await, &id).txid, Some(txid.clone()));
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Withdrawing { txid: Some(txid) }),
        "on its way until it lands in Saving"
    );
    assert!(
        !drain(&events)
            .await
            .iter()
            .any(|event| matches!(event, Event::Notification(Notice::SendConfirmed { .. }))),
        "an exit is no send"
    );
    assert!(backend.operations().await.unwrap().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_now_shows_the_fee_and_ignores_the_fee_guard() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    // 5% of the denomination, above the 2% guard.
    let fee = sat(50_000);
    saving.harness.first.set_withdraw_fee(fee);
    let item = item_of(&*backend, mbtc(10)).await;
    let (preview, _) = exit_item(&backend, &item).await;
    assert_eq!(preview.fee, fee);
    wait_until("the withdrawal", || async {
        saving.harness.first.withdrawn().len() == 1
    })
    .await;
    assert_eq!(saving.harness.first.withdrawn()[0].fee, fee);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_now_is_refused_in_the_grace_period() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    let preview = backend.prepare_exit(item.clone()).await.unwrap();
    // The default grace period: the federation came online moments ago.
    backend
        .set_settings(chain_settings(CHAIN_URL))
        .await
        .unwrap();
    assert_eq!(
        backend.exit_now(preview.id).await,
        Err(FedimixError::GracePeriod)
    );
    assert_eq!(
        backend.prepare_exit(item.clone()).await,
        Err(FedimixError::GracePeriod)
    );
    assert_eq!(saving.harness.first.withdraw_calls(), 0);
    assert!(exit_addresses_of(&saving.harness, &item).is_empty());
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Queued)
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_now_is_refused_without_saving_or_federation() {
    let saving = SavingHarness::new();
    saving.harness.first.set_balance(mbtc(60));
    let backend = saving.with_server().await;
    backend.set_settings(exit_settings()).await.unwrap();
    backend.join_federation(INVITE.into()).await.unwrap();
    wait_queue(&*backend, &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    assert_eq!(
        backend.prepare_exit(item.clone()).await,
        Err(FedimixError::NotSetUp(SET_UP_FIRST.into()))
    );
    assert!(matches!(
        backend.prepare_exit("no such item".into()).await,
        Err(FedimixError::UnknownItem(_))
    ));
    saving.chain.push_full_scan(first_scan(1));
    backend.setup_saving(SOFTWARE.into()).await.unwrap();
    wait_chain(&*backend, true).await;
    saving.harness.first.set_online(false);
    wait_online(&*backend, false).await;
    assert_eq!(
        backend.prepare_exit(item.clone()).await,
        Err(FedimixError::Offline)
    );
    saving.harness.first.set_online(true);
    wait_online(&*backend, true).await;
    // For spending is 0.05 BTC: a fee above it cannot be paid.
    saving.harness.first.set_withdraw_fee(mbtc(51));
    assert!(matches!(
        backend.prepare_exit(item.clone()).await,
        Err(FedimixError::InsufficientFunds { .. })
    ));
    saving.harness.first.set_withdraw_fee(EXIT_FEE);
    exit_item(&backend, &item).await;
    // Withdrawing is no longer an item that can exit.
    assert!(matches!(
        backend.prepare_exit(item.clone()).await,
        Err(FedimixError::UnknownItem(_))
    ));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_now_never_charges_more_than_shown() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    let preview = backend.prepare_exit(item.clone()).await.unwrap();
    saving.harness.first.set_withdraw_fee(sat(2_001));
    assert_eq!(
        backend.exit_now(preview.id).await,
        Err(FedimixError::PreviewExpired)
    );
    assert_eq!(saving.harness.first.withdraw_calls(), 0);
    assert!(exit_addresses_of(&saving.harness, &item).is_empty());
    assert!(
        !rows(&*backend)
            .await
            .iter()
            .any(|row| matches!(row.kind, HistoryKind::ToSaving { .. })),
        "no row for an exit that never started"
    );
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Queued)
    );

    let preview = backend.prepare_exit(item.clone()).await.unwrap();
    assert_eq!(preview.fee, sat(2_001));
    saving.harness.first.set_withdraw_fee(sat(1_000));
    let id = backend.exit_now(preview.id).await.unwrap();
    wait_until("the withdrawal", || async {
        saving.harness.first.withdrawn().len() == 1
    })
    .await;
    assert_eq!(saving.harness.first.withdrawn()[0].fee, sat(1_000));
    assert_eq!(row_of(&rows(&*backend).await, &id).fee, Some(sat(1_000)));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_exit_lands_labelled_as_early() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    // Someone else's deposit after the item's origin. Written to the store,
    // as this harness syncs sessions at the default pace.
    let origin = origins(&saving.harness)[0].1;
    raw_store(&saving.harness)
        .execute(
            "INSERT INTO anon_deposits
                (federation_id, session, position, source, amount_msat, reference)
             VALUES (?1, ?2, 0, 'on_chain', 2000000000, ?3)",
            rusqlite::params![FIRST_FEDERATION, origin as i64 + 1, outpoint(1)],
        )
        .unwrap();
    let estimate = item_estimate(&backend, FIRST_FEDERATION, origin, mbtc(10)).unwrap();
    assert_eq!(estimate, 1);

    let events = backend.subscribe();
    let (_, id) = exit_item(&backend, &item).await;
    let op = operation_of(&saving.harness.first, &id).await;
    let address = saving.harness.first.withdrawn()[0].address.clone();
    let tx = payment(&address, 1_000_000, 5);
    let txid = tx.compute_txid().to_string();
    saving.harness.first.push_update(&op, exit_settled(&txid));
    run_of_kind(&events, &id, OperationKind::Exit).await;
    assert_eq!(
        backend.saving_state().await.unwrap().in_flight,
        mbtc(10),
        "on its way until it confirms"
    );

    saving.chain.push_sync(confirmed(tx, 2));
    sync_saving_now(&backend);
    wait_event(
        &events,
        &Event::Notification(Notice::ExitLanded { amount: mbtc(10) }),
    )
    .await;
    let state = backend.saving_state().await.unwrap();
    let coin = state.coins.iter().find(|c| c.address == address).unwrap();
    assert_eq!(coin.tag, PrivacyTag::EarlyExit { estimate });
    let name = backend.federations().await.unwrap().remove(0).name;
    assert_eq!(coin.source_federation, Some(name));
    assert_eq!(state.in_flight, Amount::ZERO);
    let row = state
        .history
        .iter()
        .find(|row| row.txid.as_deref() == Some(txid.as_str()))
        .unwrap();
    assert_eq!(
        row.kind,
        HistoryKind::FromSpending {
            denomination: mbtc(10)
        }
    );
    assert_eq!(
        status_of(&*backend, &item).await,
        None,
        "landed items leave"
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_exit_requeues_and_never_reuses_its_address() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    let client = &saving.harness.first;
    let queued = Some(fedimix_core::QueueItemStatus::Queued);

    // Refused at submission.
    client.fail_next_withdraw(FedimixError::Internal(EXIT_REFUSED.into()));
    let events = backend.subscribe();
    let (_, first) = exit_item(&backend, &item).await;
    let (states, _) = run_of_kind(&events, &first, OperationKind::Exit).await;
    assert_eq!(states.last(), Some(&failed_with(EXIT_REFUSED)));
    wait_status(&*backend, &first, HistoryStatus::Failed).await;
    assert_eq!(status_of(&*backend, &item).await, queued);

    // Failed once submitted.
    let (_, second) = exit_item(&backend, &item).await;
    let op = operation_of(client, &second).await;
    client.push_update(
        &op,
        WatchUpdate::Failed {
            reason: EXIT_REFUSED.into(),
        },
    );
    let (states, _) = run_of_kind(&events, &second, OperationKind::Exit).await;
    assert_eq!(states.last(), Some(&failed_with(EXIT_REFUSED)));
    wait_status(&*backend, &second, HistoryStatus::Failed).await;
    assert_eq!(status_of(&*backend, &item).await, queued);

    exit_item(&backend, &item).await;
    wait_until("the third withdrawal", || async {
        client.withdrawn().len() == 2
    })
    .await;
    let addresses = exit_addresses_of(&saving.harness, &item);
    assert_eq!(
        addresses.len(),
        3,
        "each try kept its address: {addresses:?}"
    );
    let distinct: std::collections::HashSet<_> = addresses.iter().collect();
    assert_eq!(distinct.len(), 3, "{addresses:?}");
    let paid: Vec<_> = client.withdrawn().into_iter().map(|q| q.address).collect();
    assert_eq!(paid, addresses[1..]);
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn killed_before_submit_requeues_with_a_new_address() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    halt_sends(&backend, SendHalt::BeforeSubmit);
    let (_, id) = exit_item(&backend, &item).await;
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Withdrawing { txid: None })
    );
    let first = exit_addresses_of(&saving.harness, &item);
    assert_eq!(first.len(), 1);

    let backend = saving.reopened(backend).await;
    let events = backend.subscribe();
    let (states, history_id) = run_of_kind(&events, &id, OperationKind::Exit).await;
    assert_eq!(states, [failed_with(EXIT_NEVER_SENT)]);
    assert_eq!(history_id, Some(id.clone()));
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Queued)
    );
    assert_eq!(
        saving.harness.first.withdraw_calls(),
        0,
        "nothing exits by itself"
    );

    exit_item(&backend, &item).await;
    wait_until("the withdrawal", || async {
        saving.harness.first.withdrawn().len() == 1
    })
    .await;
    let address = saving.harness.first.withdrawn()[0].address.clone();
    assert_ne!(address, first[0]);
    assert_eq!(
        exit_addresses_of(&saving.harness, &item),
        [first[0].clone(), address]
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn killed_after_submit_resumes_the_exit() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    halt_sends(&backend, SendHalt::AfterSubmit);
    let (_, id) = exit_item(&backend, &item).await;
    let op = operation_of(&saving.harness.first, &id).await;

    let backend = saving.reopened(backend).await;
    let events = backend.subscribe();
    let txid = exit_txid(2);
    saving.harness.first.push_update(&op, exit_settled(&txid));
    let (states, _) = run_of_kind(&events, &id, OperationKind::Exit).await;
    assert_eq!(states, [OperationState::Done]);
    wait_status(&*backend, &id, HistoryStatus::Complete).await;
    assert_eq!(
        saving.harness.first.withdrawn().len(),
        1,
        "exactly one withdrawal"
    );
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Withdrawing { txid: Some(txid) })
    );
    assert_eq!(exit_addresses_of(&saving.harness, &item).len(), 1);
    backend.shutdown().await.unwrap();
}

/// Review Focus 1: a send confirmed while an exit of the same federation is
/// being submitted cannot take the exit's money, and its fit leaves the exit.
#[tokio::test]
async fn a_send_and_an_exit_never_overdraw() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(80), &[mbtc(20), mbtc(10)]).await;
    let client = &saving.harness.first;
    client.add_invoice(INVOICE, mbtc(20));
    let exiting = item_of(&*backend, mbtc(10)).await;
    client.set_withdraw_held(true);
    let (_, exit) = exit_item(&backend, &exiting).await;
    wait_until("the exit's submission", || async {
        client.withdraw_calls() == 1
    })
    .await;

    let whole = backend
        .prepare_send(to(ADDRESS, SendAmount::Max))
        .await
        .unwrap();
    assert!(matches!(
        backend.confirm_send(whole.id).await,
        Err(FedimixError::InsufficientFunds { .. })
    ));

    let pay = backend.prepare_send(pay_request()).await.unwrap();
    backend.confirm_send(pay.id).await.unwrap();
    wait_until("the fit", || async {
        queue_ids(&*backend).await == [exiting.clone()]
    })
    .await;
    assert_eq!(
        status_of(&*backend, &exiting).await,
        Some(fedimix_core::QueueItemStatus::Withdrawing { txid: None })
    );

    client.set_withdraw_held(false);
    operation_of(client, &exit).await;
    assert_eq!(client.withdrawn().len(), 1);
    assert_eq!(client.paid(), [INVOICE]);
    let left = mbtc(80)
        .checked_sub(mbtc(30))
        .and_then(|left| left.checked_sub(EXIT_FEE))
        .and_then(|left| left.checked_sub(sat(10)))
        .unwrap();
    wait_balance(&*backend, left).await;
    backend.shutdown().await.unwrap();
}

/// Review Focus 4: a coin that arrives while a sync hangs is labelled once
/// the sync ends, as labels are read by address.
#[tokio::test]
async fn a_coin_landing_during_a_scan_is_labelled_after_it() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    let events = backend.subscribe();
    let (_, id) = exit_item(&backend, &item).await;
    let op = operation_of(&saving.harness.first, &id).await;
    let address = saving.harness.first.withdrawn()[0].address.clone();
    let tx = payment(&address, 1_000_000, 6);
    saving
        .harness
        .first
        .push_update(&op, exit_settled(&tx.compute_txid().to_string()));
    run_of_kind(&events, &id, OperationKind::Exit).await;

    saving.chain.set_stalled(true);
    saving.chain.push_sync(confirmed(tx, 2));
    sync_saving_now(&backend);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        matches!(
            status_of(&*backend, &item).await,
            Some(fedimix_core::QueueItemStatus::Withdrawing { .. })
        ),
        "not landed while the sync hangs"
    );
    saving.chain.set_stalled(false);
    wait_until("the labelled coin", || async {
        sync_saving_now(&backend);
        let state = backend.saving_state().await.unwrap();
        state
            .coins
            .iter()
            .any(|c| c.address == address && matches!(c.tag, PrivacyTag::EarlyExit { .. }))
    })
    .await;
    wait_until("the landing", || async {
        status_of(&*backend, &item).await.is_none()
    })
    .await;
    backend.shutdown().await.unwrap();
}

const EXIT_UNKNOWN: &str = "Fedimix cannot tell whether this exit was sent. If it was, it lands in Saving; if not, the money stays in Spending.";
const EXIT_LOST_RECORD: &str = "Fedimix lost its record of this exit and cannot tell whether it was sent. If it was, it lands in Saving; if not, the money stays in Spending.";

/// v2 fails a withdrawal it had accepted, with no transaction: the money
/// may have left, so the item must not go out again, and its coin still
/// lands it.
#[tokio::test]
async fn an_exit_whose_outcome_is_unknown_is_not_requeued() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    let events = backend.subscribe();
    let (_, id) = exit_item(&backend, &item).await;
    let op = operation_of(&saving.harness.first, &id).await;
    saving.harness.first.push_update(
        &op,
        WatchUpdate::Unknown {
            reason: "The federation accepted this withdrawal but reported no transaction, so the money may have left the wallet.".into(),
        },
    );
    let (states, _) = run_of_kind(&events, &id, OperationKind::Exit).await;
    assert_eq!(states.last(), Some(&failed_with(EXIT_UNKNOWN)));
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Withdrawing { txid: None })
    );
    assert!(matches!(
        backend.prepare_exit(item.clone()).await,
        Err(FedimixError::UnknownItem(_))
    ));
    assert_eq!(saving.harness.first.withdraw_calls(), 1);

    let address = saving.harness.first.withdrawn()[0].address.clone();
    saving
        .chain
        .push_sync(confirmed(payment(&address, 1_000_000, 8), 2));
    sync_saving_now(&backend);
    wait_event(
        &events,
        &Event::Notification(Notice::ExitLanded { amount: mbtc(10) }),
    )
    .await;
    assert_eq!(status_of(&*backend, &item).await, None);
    backend.shutdown().await.unwrap();
}

/// A submitted exit not yet settled when the client database is lost: the
/// rebuilt client cannot follow it, so it ends unknown rather than requeue.
#[tokio::test]
async fn a_lost_database_with_a_submitted_exit_does_not_requeue() {
    let saving = SavingHarness::new();
    let backend = saving.exiting(mbtc(60), &[mbtc(10)]).await;
    let item = item_of(&*backend, mbtc(10)).await;
    let (_, id) = exit_item(&backend, &item).await;
    let op = operation_of(&saving.harness.first, &id).await;
    wait_operation_on_disk(&saving.harness, &id, &op).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    let port = &saving.harness.port;
    port.set_open_error(Some(FedimixError::UnknownItem("one".into())));
    port.set_recover_held(true);
    let backend = saving.open().await.unwrap();
    let events = backend.subscribe();
    let (states, _) = run_of_kind(&events, &id, OperationKind::Exit).await;
    assert_eq!(states, [failed_with(EXIT_LOST_RECORD)]);
    wait_status(&*backend, &id, HistoryStatus::Failed).await;
    assert_eq!(
        status_of(&*backend, &item).await,
        Some(fedimix_core::QueueItemStatus::Withdrawing { txid: None })
    );
    assert_eq!(saving.harness.first.withdraw_calls(), 1);
    port.set_recover_held(false);
    backend.shutdown().await.unwrap();
}
