//! Tests against a devimint federation. Run through `just test-integration`,
//! which starts devimint for both module generations.
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use fedimint_bip39::Bip39RootSecretStrategy;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, RootSecret};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::PeerId;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::util::SafeUrl;
use fedimix_core::testing::{
    DataDirLock, DepositEvent, FederationClient, FederationPort, FedimintPort, Seed, SendHalt,
    Watch, WatchKind, WatchUpdate, halt_sends,
};
use fedimix_core::{
    Amount, Backend, CoreConfig, Event, EventStream, FedimixError, HistoryItem, HistoryStatus,
    Network, Notice, OperationState, RealBackend, ReceiveInfo, ReceiveMethod, ReceiveRequest,
    SendAmount, SendPreview, SendRequest, WalletKind,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use tempfile::TempDir;

/// Which module generation the federation under test must run, chosen by
/// `scripts/test-integration.sh` through `FEDIMIX_EXPECT_MODULES=v1|v2`.
/// Panics if the variable is unset, so a test cannot run without choosing one.
fn assert_expected_generation(kinds: &[String]) {
    let has = |k: &str| kinds.iter().any(|x| x == k);
    let (present, absent) = match std::env::var("FEDIMIX_EXPECT_MODULES").as_deref() {
        Ok("v1") => (["mint", "wallet"], ["mintv2", "walletv2"]),
        Ok("v2") => (["mintv2", "walletv2"], ["mint", "wallet"]),
        other => panic!("FEDIMIX_EXPECT_MODULES must be v1 or v2, got {other:?}"),
    };
    for k in present {
        assert!(has(k), "expected module {k} in {kinds:?}");
    }
    for k in absent {
        assert!(!has(k), "unexpected module {k} in {kinds:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn smoke_join_and_read_modules() {
    let invite = std::env::var("FM_INVITE_CODE").expect("FM_INVITE_CODE is set by devimint");
    let dir = tempfile::tempdir().unwrap();

    let mnemonic = Bip39RootSecretStrategy::<12>::random(&mut rand::thread_rng());
    let connectors = ConnectorRegistry::build_from_client_defaults().bind().await;
    let mut builder = Client::builder().await;
    builder.with_module(fedimint_lnv2_client::LightningClientInit::default());
    builder.with_module(fedimint_mint_client::MintClientInit);
    builder.with_module(fedimint_mintv2_client::MintClientInit);
    builder.with_module(fedimint_wallet_client::WalletClientInit(None));
    builder.with_module(fedimint_walletv2_client::WalletClientInit);

    let preview = builder
        .preview(connectors, &InviteCode::from_str(&invite).unwrap())
        .await
        .unwrap();
    let kinds: Vec<String> = preview
        .config()
        .modules
        .values()
        .map(|m| m.kind().to_string())
        .collect();
    eprintln!("module kinds: {kinds:?}");
    assert!(kinds.iter().any(|k| k == "lnv2"));
    assert_expected_generation(&kinds);
    // No client module is registered for `meta`: an unknown extra kind must not
    // stop the join below.
    assert!(kinds.iter().any(|k| k == "meta"));

    let db = fedimint_rocksdb::RocksDb::build(dir.path().join("client.rocksdb"))
        .open()
        .await
        .unwrap()
        .into();
    let client = Arc::new(
        preview
            .join(
                db,
                RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<12>::to_root_secret(
                    &mnemonic,
                )),
            )
            .await
            .unwrap(),
    );
    assert_eq!(client.get_balance_for_btc().await.unwrap().msats, 0);
}

/// How long a test waits for something devimint does within seconds.
const PATIENCE: Duration = Duration::from_secs(120);

fn sh(cmd: &str) -> Result<String, String> {
    let out = Command::new("bash").arg("-c").arg(cmd).output().unwrap();
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn btc(args: &str) -> String {
    sh(&format!("$FM_BTC_CLIENT -rpcwallet=default {args}")).unwrap()
}

fn mine(n: u32) {
    let address = btc("getnewaddress");
    btc(&format!("generatetoaddress {n} {address}"));
}

fn fund(address: &str, sat: u64) {
    let amount = format!("{}.{:08}", sat / 100_000_000, sat % 100_000_000);
    btc(&format!(
        "-named sendtoaddress address={address} amount={amount}"
    ));
}

fn lnd_invoice(sat: u64) -> String {
    let reply = sh(&format!("$FM_LNCLI addinvoice --amt {sat}")).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    reply["payment_request"].as_str().unwrap().to_string()
}

/// Pays with a new invoice each try. The product picks a gateway at random, and
/// LND cannot pay an invoice from its own gateway ("self-payments not allowed").
/// The other two work: `scripts/fund-gateways.sh` gives each ecash.
async fn pay_with_lnd(client: &dyn FederationClient, amount: Amount) -> String {
    const TRIES: u32 = 8;
    for attempt in 1..=TRIES {
        let (invoice, watch_id) = client
            .lightning_invoice(amount, "test", 3600)
            .await
            .unwrap();
        match sh(&format!(
            "timeout 30 $FM_LNCLI payinvoice --force {invoice}"
        )) {
            Ok(_) => return watch_id,
            Err(e) => eprintln!("payinvoice attempt {attempt} failed: {e}"),
        }
    }
    panic!("LND could not pay {TRIES} invoices");
}

/// A fresh wallet in its own data folder.
struct Wallet {
    dir: TempDir,
    _lock: DataDirLock,
    seed: Seed,
    port: FedimintPort,
}

impl Wallet {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = DataDirLock::acquire(dir.path()).unwrap();
        let seed = Seed::load_or_create(dir.path(), &lock).unwrap();
        let port = FedimintPort::new(dir.path());
        Self {
            dir,
            _lock: lock,
            seed,
            port,
        }
    }

    async fn join(&self) -> Arc<dyn FederationClient> {
        self.port.join(&invite(), &self.seed).await.unwrap()
    }

    /// Shuts `client` down and opens it again, as a restart does.
    async fn restart(&self, client: Arc<dyn FederationClient>) -> Arc<dyn FederationClient> {
        let id = client.id();
        client.shutdown().await.unwrap();
        self.port.open(&id, &self.seed).await.unwrap()
    }
}

fn invite() -> String {
    std::env::var("FM_INVITE_CODE").expect("FM_INVITE_CODE is set by devimint")
}

fn watch(id: &str, kind: WatchKind) -> Watch {
    Watch {
        id: id.to_string(),
        federation_id: String::new(),
        kind,
        created_at: 0,
    }
}

/// Reads to the end of a watch; panics if the last update is not final.
async fn until_final(stream: BoxStream<'static, WatchUpdate>) -> WatchUpdate {
    let updates: Vec<WatchUpdate> = tokio::time::timeout(PATIENCE, stream.collect())
        .await
        .expect("the watch must end");
    let last = updates.last().expect("a watch yields its final update");
    assert!(last.is_final(), "{updates:?}");
    last.clone()
}

/// A joined wallet holding about `sat`, received over Lightning.
async fn funded(wallet: &Wallet, sat: u64) -> Arc<dyn FederationClient> {
    let client = wallet.join().await;
    let id = pay_with_lnd(&*client, Amount::from_sat(sat)).await;
    let update = until_final(client.watch(watch(&id, WatchKind::LnReceive))).await;
    assert!(matches!(update, WatchUpdate::Settled { .. }), "{update:?}");
    client
}

/// Mines a block a second until `stream` yields something `done` accepts.
async fn mine_until<T: std::fmt::Debug>(
    mut stream: BoxStream<'static, T>,
    done: impl Fn(&T) -> bool,
) -> T {
    let found = tokio::time::timeout(PATIENCE, async {
        loop {
            tokio::select! {
                item = stream.next() => {
                    let item = item.expect("the stream ended early");
                    eprintln!("update: {item:?}");
                    if done(&item) {
                        return item;
                    }
                }
                () = tokio::time::sleep(Duration::from_secs(1)) => mine(1),
            }
        }
    })
    .await;
    found.expect("nothing arrived while mining")
}

/// The federation broadcasts a moment after the client learns the txid.
async fn wait_for_transaction(txid: &str) {
    let lookup = format!("$FM_BTC_CLIENT -rpcwallet=default gettransaction {txid}");
    tokio::time::timeout(PATIENCE, async {
        while sh(&lookup).is_err() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .expect("the transaction must reach bitcoind");
}

/// Quotes `invoice` and pays it.
async fn pay(client: &dyn FederationClient, invoice: &str, amount: Amount) -> String {
    let quote = client.lightning_quote(invoice).await.unwrap();
    assert_eq!(quote.amount, amount);
    assert!(quote.fee > Amount::ZERO);
    client.pay_invoice(&quote, "test").await.unwrap()
}

/// Never above what was shown; at most 1 sat below it.
fn within_shown(spent: Amount, shown: Amount) -> bool {
    spent <= shown && shown.saturating_sub(spent) <= Amount::from_sat(1)
}

fn is_sentence(text: &str) -> bool {
    text.ends_with('.') && !text.contains("Error") && !text.contains('{')
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn join_reopen_keeps_identity() {
    let wallet = Wallet::new();
    let preview = wallet.port.preview(&invite()).await.unwrap();
    assert_eq!(preview.network, Some(Network::Regtest));
    assert_eq!(wallet.port.federation_id_of(&invite()).unwrap(), preview.id);
    assert!(preview.has_lightning);
    assert!(!preview.name.is_empty());

    let client = wallet.join().await;
    assert_eq!(client.id(), preview.id);
    assert_eq!(client.name(), preview.name);
    assert_eq!(client.balance().await.unwrap(), Amount::ZERO);
    let db = wallet
        .dir
        .path()
        .join("federations")
        .join(&preview.id)
        .join("client");
    assert!(db.is_dir(), "{db:?}");

    let mut online = client.online_changes();
    tokio::time::timeout(PATIENCE, async {
        while online.next().await != Some(true) {}
    })
    .await
    .expect("a threshold of guardians connects");

    let reopened = wallet.restart(client).await;
    assert_eq!(reopened.id(), preview.id);
    assert_eq!(reopened.name(), preview.name);
    assert_eq!(reopened.balance().await.unwrap(), Amount::ZERO);
    assert_eq!(
        online.next().await,
        None,
        "shutdown ends the streams of the old client"
    );

    // Joining again must reach the same wallet, not fail or start over.
    reopened.shutdown().await.unwrap();
    let again = wallet.join().await;
    assert_eq!(again.id(), preview.id);
    again.shutdown().await.unwrap();

    let missing = wallet.port.open("nothing-here", &wallet.seed).await;
    assert_eq!(
        missing.err(),
        Some(FedimixError::UnknownItem("nothing-here".into()))
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn deposit_credits_balance() {
    const DEPOSIT_SAT: u64 = 100_000;
    let wallet = Wallet::new();
    let client = wallet.join().await;
    let deposit = Amount::from_sat(DEPOSIT_SAT);
    let (address, watch_id) = client.deposit_address().await.unwrap();
    fund(&address, DEPOSIT_SAT);

    let (credited, fee) = match &watch_id {
        Some(id) => {
            let stream = client.watch(watch(id, WatchKind::Deposit));
            match mine_until(stream, WatchUpdate::is_final).await {
                WatchUpdate::Settled { amount, fee, txid } => {
                    assert!(txid.is_some());
                    (amount, fee)
                }
                other => panic!("{other:?}"),
            }
        }
        None => {
            let event = mine_until(client.deposits(0), |e: &DepositEvent| e.confirmed).await;
            assert!(event.txid.is_some());
            (event.amount, event.fee)
        }
    };
    assert_eq!(credited.checked_add(fee), Some(deposit));
    assert!(fee > Amount::ZERO);

    // The ecash fee comes off what the operation reports.
    let balance = client.balance().await.unwrap();
    assert!(balance <= credited, "{balance:?} {credited:?}");
    assert!(
        credited.saturating_sub(balance) < Amount::from_sat(1_000),
        "{balance:?} {credited:?}"
    );

    // After a restart the finished deposit is still reported, and only once.
    let client = wallet.restart(client).await;
    assert_eq!(client.balance().await.unwrap(), balance);
    match &watch_id {
        Some(id) => {
            let update = until_final(client.watch(watch(id, WatchKind::Deposit))).await;
            assert!(matches!(update, WatchUpdate::Settled { .. }), "{update:?}");
            assert_eq!(client.deposits(0).next().await, None);
        }
        None => {
            let replay = client
                .deposits(0)
                .filter(|e| std::future::ready(e.confirmed));
            let event = std::pin::pin!(replay).next().await.unwrap();
            let mut after = client.deposits(event.cursor);
            let nothing = tokio::time::timeout(Duration::from_secs(3), after.next()).await;
            assert!(nothing.is_err(), "replayed: {nothing:?}");
        }
    }
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn lightning_receive_credits_balance() {
    let wallet = Wallet::new();
    let client = wallet.join().await;
    let invoiced = Amount::from_sat(5_000);
    let id = pay_with_lnd(&*client, invoiced).await;

    let update = until_final(client.watch(watch(&id, WatchKind::LnReceive))).await;
    let WatchUpdate::Settled { amount, fee, txid } = update.clone() else {
        panic!("{update:?}");
    };
    assert_eq!(amount.checked_add(fee), Some(invoiced));
    assert_eq!(txid, None);
    let balance = client.balance().await.unwrap();
    assert!(balance > Amount::ZERO && balance <= amount, "{balance:?}");
    assert!(!client.notes().await.unwrap().is_empty());

    // A watch started anew after the restart yields the same final state.
    let client = wallet.restart(client).await;
    assert_eq!(
        until_final(client.watch(watch(&id, WatchKind::LnReceive))).await,
        update
    );
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn pay_invoice_settles() {
    const LND_TRIES: u32 = 3;
    const TRIES: u32 = 5;
    let wallet = Wallet::new();
    let client = funded(&wallet, 20_000).await;
    let other = Wallet::new();
    let other_client = other.join().await;
    let paid = Amount::from_sat(2_000);

    // With no gateway named, an LND invoice goes to LND's own gateway. Its node
    // issued the invoice, so it looks for a direct swap, finds none and refunds
    // without trying Lightning. After those tries, pay another wallet on the
    // federation. A refund must bring the money back.
    let mut settled = None;
    for attempt in 1..=TRIES {
        let invoice = if attempt <= LND_TRIES {
            lnd_invoice(paid.as_sat())
        } else {
            let (invoice, _) = other_client
                .lightning_invoice(paid, "test", 3600)
                .await
                .unwrap();
            invoice
        };
        let before = client.balance().await.unwrap();
        let op = pay(&*client, &invoice, paid).await;
        match until_final(client.watch(watch(&op, WatchKind::LnSend))).await {
            WatchUpdate::Settled { amount, .. } => {
                assert_eq!(amount, paid);
                eprintln!("SETTLED on attempt {attempt}");
                settled = Some((op, before));
                break;
            }
            WatchUpdate::Refunded => {
                eprintln!("REFUNDED on attempt {attempt}");
                let restored = client.balance().await.unwrap();
                assert!(
                    restored >= before.saturating_sub(Amount::from_sat(100)),
                    "{restored:?} of {before:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }
    let (id, before) = settled.expect("no attempt settled");
    let spent = before.saturating_sub(client.balance().await.unwrap());
    assert!(spent >= paid, "{spent:?}");
    assert!(spent < Amount::from_sat(2_500), "{spent:?}");

    let client = wallet.restart(client).await;
    let update = until_final(client.watch(watch(&id, WatchKind::LnSend))).await;
    assert!(matches!(update, WatchUpdate::Settled { .. }), "{update:?}");
    client.shutdown().await.unwrap();
    other_client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn withdraw_returns_txid() {
    let wallet = Wallet::new();
    let client = funded(&wallet, 100_000).await;
    let sent = Amount::from_sat(50_000);
    let address = btc("getnewaddress");
    let before = client.balance().await.unwrap();

    let quote = client
        .withdraw_quote(&address, SendAmount::Exact(sent))
        .await
        .unwrap();
    assert_eq!(quote.amount, sent);
    assert!(quote.fee > Amount::ZERO);

    let max = client
        .withdraw_quote(&address, SendAmount::Max)
        .await
        .unwrap();
    assert!(max.amount > sent);
    assert!(max.amount.checked_add(max.fee).unwrap() <= before);

    let id = client.withdraw(&quote, "test").await.unwrap();
    let update = until_final(client.watch(watch(&id, WatchKind::Withdraw))).await;
    let WatchUpdate::Settled { amount, txid, .. } = update.clone() else {
        panic!("{update:?}");
    };
    assert_eq!(amount, sent);
    let txid = txid.expect("a withdrawal has a transaction");
    wait_for_transaction(&txid).await;

    let spent = before.saturating_sub(client.balance().await.unwrap());
    let shown = quote.amount.checked_add(quote.fee).unwrap();
    assert!(
        within_shown(spent, shown),
        "spent {spent:?}, shown {shown:?}"
    );

    let client = wallet.restart(client).await;
    assert_eq!(
        until_final(client.watch(watch(&id, WatchKind::Withdraw))).await,
        update
    );
    let wrong_network = client
        .withdraw_quote(
            "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
            SendAmount::Exact(sent),
        )
        .await;
    assert!(
        matches!(wrong_network, Err(FedimixError::InvalidDestination(_))),
        "{wrong_network:?}"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn min_deposit_is_positive() {
    let wallet = Wallet::new();
    let client = wallet.join().await;
    assert!(client.min_deposit().await.unwrap() > Amount::ZERO);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn unknown_invite_is_a_sentence() {
    let wallet = Wallet::new();
    match wallet.port.preview("fed1nonsense").await {
        Err(FedimixError::InvalidInvite(text)) => assert!(is_sentence(&text), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(
        wallet
            .port
            .join("not an invite", &wallet.seed)
            .await
            .is_err()
    );

    // Well formed, but nobody answers there.
    let real = InviteCode::from_str(&invite()).unwrap();
    let dead = InviteCode::new(
        SafeUrl::parse("ws://127.0.0.1:9/").unwrap(),
        PeerId::from(0),
        real.federation_id(),
        None,
    );
    let reply = wallet.port.preview(&dead.to_string()).await;
    assert_eq!(reply.err(), Some(FedimixError::Offline));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn deposit_at_or_below_fee_is_not_settled() {
    let wallet = Wallet::new();
    let client = wallet.join().await;
    let (address, watch_id) = client.deposit_address().await.unwrap();
    // Only a generation with deposit watches can report a final state here: the
    // other one never logs a deposit it cannot claim.
    let Some(id) = watch_id else {
        return;
    };
    fund(&address, client.min_deposit().await.unwrap().as_sat());
    let stream = client.watch(watch(&id, WatchKind::Deposit));
    match mine_until(stream, WatchUpdate::is_final).await {
        WatchUpdate::Failed { reason } => assert!(is_sentence(&reason), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(client.balance().await.unwrap(), Amount::ZERO);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn changed_gateway_terms_refuse_the_payment() {
    let wallet = Wallet::new();
    let client = funded(&wallet, 10_000).await;
    let before = client.balance().await.unwrap();
    let mut quote = client.lightning_quote(&lnd_invoice(1_000)).await.unwrap();
    quote.terms.fee_ppm += 1;
    assert_eq!(
        client.pay_invoice(&quote, "test").await.err(),
        Some(FedimixError::PreviewExpired)
    );
    assert_eq!(client.balance().await.unwrap(), before);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn find_operation_locates_a_payment() {
    let wallet = Wallet::new();
    let client = funded(&wallet, 100_000).await;
    assert_eq!(client.find_operation("never-sent").await.unwrap(), None);

    let quote = client.lightning_quote(&lnd_invoice(2_000)).await.unwrap();
    let paid = client.pay_invoice(&quote, "intent-pay").await.unwrap();
    let address = btc("getnewaddress");
    let quote = client
        .withdraw_quote(&address, SendAmount::Exact(Amount::from_sat(20_000)))
        .await
        .unwrap();
    let withdrawn = client.withdraw(&quote, "intent-withdraw").await.unwrap();

    let client = wallet.restart(client).await;
    assert_eq!(
        client.find_operation("intent-pay").await.unwrap(),
        Some(paid)
    );
    assert_eq!(
        client.find_operation("intent-withdraw").await.unwrap(),
        Some(withdrawn)
    );
    assert_eq!(client.find_operation("never-sent").await.unwrap(), None);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn withdraw_quote_reports_the_real_problem() {
    let wallet = Wallet::new();
    let client = wallet.join().await;
    let address = btc("getnewaddress");

    // More than the federation holds is about the balance, not the connection.
    let reply = client
        .withdraw_quote(&address, SendAmount::Exact(Amount::from_sat(1 << 50)))
        .await;
    assert!(
        matches!(reply, Err(FedimixError::InsufficientFunds { .. })),
        "{reply:?}"
    );

    // Dust is refused before anyone confirms it.
    let reply = client
        .withdraw_quote(&address, SendAmount::Exact(Amount::from_sat(100)))
        .await;
    assert!(
        matches!(reply, Err(FedimixError::InvalidAmount(_))),
        "{reply:?}"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn restart_keeps_federation_and_balance() {
    // Funded through the port. Joining then finds the client database and opens
    // it.
    let wallet = Wallet::new();
    let client = funded(&wallet, 10_000).await;
    let balance = client.balance().await.unwrap();
    assert!(balance > Amount::ZERO);
    let id = client.id();
    client.shutdown().await.unwrap();
    let Wallet { dir, _lock, .. } = wallet;
    drop(_lock);
    let config = CoreConfig {
        data_dir: dir.path().to_str().unwrap().to_string(),
        network: Network::Regtest,
    };

    let backend = RealBackend::open(config.clone()).await.unwrap();
    let joined = backend.join_federation(invite()).await.unwrap();
    assert_eq!(joined.id, id);
    connected_with(&backend, balance).await;
    backend.shutdown().await.unwrap();
    drop(backend);

    let backend = RealBackend::open(config).await.unwrap();
    // Before anything has connected: this is what the store remembers.
    let state = backend.spending_state().await.unwrap();
    assert_eq!(state.balance, balance);
    assert_eq!(state.federation.unwrap().id, id);
    connected_with(&backend, balance).await;
    backend.shutdown().await.unwrap();
    backend.shutdown().await.unwrap();
}

async fn connected_with(backend: &RealBackend, balance: Amount) {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let state = backend.spending_state().await.unwrap();
            if state.balance == balance
                && !state.notes.is_empty()
                && state.federation.is_some_and(|f| f.online)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the federation connects with its balance");
}

fn folder() -> (TempDir, CoreConfig) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = CoreConfig {
        data_dir: dir.path().to_str().unwrap().to_string(),
        network: Network::Regtest,
    };
    (dir, config)
}

/// A backend in `config`'s folder, connected to the federation.
async fn joined_backend(config: &CoreConfig) -> Arc<RealBackend> {
    let backend = RealBackend::open(config.clone()).await.unwrap();
    backend.join_federation(invite()).await.unwrap();
    wait_online(&backend).await;
    backend
}

async fn wait_online(backend: &RealBackend) {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let state = backend.spending_state().await.unwrap();
            if state.federation.is_some_and(|f| f.online) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the federation connects");
}

async fn invoice(backend: &RealBackend, sat: u64) -> ReceiveInfo {
    let request = ReceiveRequest {
        wallet: WalletKind::Spending,
        method: ReceiveMethod::Lightning {
            amount: Amount::from_sat(sat),
            description: "test".into(),
            expiry_secs: 3600,
        },
    };
    backend.receive(request).await.unwrap()
}

async fn address(backend: &RealBackend) -> ReceiveInfo {
    let request = ReceiveRequest {
        wallet: WalletKind::Spending,
        method: ReceiveMethod::OnChain,
    };
    backend.receive(request).await.unwrap()
}

/// Pays with a new invoice each try, as [`pay_with_lnd`] does.
async fn pay_backend_with_lnd(backend: &RealBackend, sat: u64) {
    const TRIES: u32 = 8;
    for attempt in 1..=TRIES {
        let info = invoice(backend, sat).await;
        assert_eq!(info.uri, format!("lightning:{}", info.payload));
        let pay = format!("timeout 30 $FM_LNCLI payinvoice --force {}", info.payload);
        match sh(&pay) {
            Ok(_) => return,
            Err(e) => eprintln!("payinvoice attempt {attempt} failed: {e}"),
        }
    }
    panic!("LND could not pay {TRIES} invoices");
}

async fn rows(backend: &RealBackend) -> Vec<HistoryItem> {
    let page = backend.history(WalletKind::Spending, None, 100).await;
    page.unwrap().items
}

async fn completed(backend: &RealBackend) -> Option<HistoryItem> {
    let rows = rows(backend).await;
    rows.into_iter()
        .find(|row| row.status == HistoryStatus::Complete)
}

/// Waits for a `Complete` row and its `Notice`, and that there is one of each.
async fn wait_received(backend: &RealBackend, events: &EventStream) -> HistoryItem {
    let notice = tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(Event::Notification(Notice::DepositConfirmed { amount })) =
                events.next().await
            {
                return amount;
            }
        }
    })
    .await
    .expect("the notice arrives");
    let row = completed(backend)
        .await
        .expect("the row precedes the notice");
    assert_eq!(row.amount, notice);
    row
}

/// As [`wait_received`], mining a block a second.
async fn mine_until_received(backend: &RealBackend, events: &EventStream) -> HistoryItem {
    let waiting = wait_received(backend, events);
    let mut waiting = std::pin::pin!(waiting);
    loop {
        tokio::select! {
            row = &mut waiting => return row,
            () = tokio::time::sleep(Duration::from_secs(1)) => mine(1),
        }
    }
}

async fn wait_balance(backend: &RealBackend, wanted: impl Fn(Amount) -> bool) -> Amount {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let balance = backend.spending_state().await.unwrap().balance;
            if wanted(balance) {
                return balance;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the balance arrives")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn receive_lightning_end_to_end() {
    let (_dir, config) = folder();
    let backend = joined_backend(&config).await;
    let events = backend.subscribe();
    let paid = Amount::from_sat(5_000);
    pay_backend_with_lnd(&backend, paid.as_sat()).await;

    let row = wait_received(&backend, &events).await;
    assert!(row.incoming);
    assert!(row.amount <= paid && row.amount > Amount::ZERO, "{row:?}");
    let balance = wait_balance(&backend, |balance| balance > Amount::ZERO).await;
    assert!(balance <= row.amount, "{balance:?} {row:?}");
    backend.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn receive_onchain_end_to_end() {
    const DEPOSIT_SAT: u64 = 100_000;
    let (_dir, config) = folder();
    let backend = joined_backend(&config).await;
    let events = backend.subscribe();
    let info = address(&backend).await;
    assert_eq!(info.uri, format!("bitcoin:{}", info.payload));
    assert!(info.min_deposit.unwrap() > Amount::ZERO);
    assert!(
        rows(&backend).await.is_empty(),
        "an address alone adds no row"
    );

    fund(&info.payload, DEPOSIT_SAT);
    let row = mine_until_received(&backend, &events).await;
    assert!(row.amount < Amount::from_sat(DEPOSIT_SAT), "{row:?}");
    let balance = wait_balance(&backend, |balance| balance > Amount::ZERO).await;
    assert!(balance <= row.amount, "{balance:?} {row:?}");
    backend.shutdown().await.unwrap();
}

/// The folder opens again once a dropped backend has let go of it.
async fn reopened_after_drop(config: &CoreConfig) -> Arc<RealBackend> {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match RealBackend::open(config.clone()).await {
                Ok(backend) => return backend,
                Err(e) => eprintln!("not yet: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("a dropped backend lets go of its folder")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn killed_mid_deposit_recovers() {
    const DEPOSIT_SAT: u64 = 100_000;

    // No watch at all before the restart, so the state machines' replay answers
    // it, not an outcome the operation log kept.
    let wallet = Wallet::new();
    let client = wallet.join().await;
    let (deposit, watch_id) = client.deposit_address().await.unwrap();
    fund(&deposit, DEPOSIT_SAT);
    let client = wallet.restart(client).await;
    match &watch_id {
        Some(id) => {
            let stream = client.watch(watch(id, WatchKind::Deposit));
            let update = mine_until(stream, WatchUpdate::is_final).await;
            assert!(matches!(update, WatchUpdate::Settled { .. }), "{update:?}");
        }
        None => {
            mine_until(client.deposits(0), |e: &DepositEvent| e.confirmed).await;
        }
    }
    assert!(client.balance().await.unwrap() > Amount::ZERO);
    client.shutdown().await.unwrap();

    // Through the backend: its watch or log reader stops with the deposit in
    // mid-air, and the row turns up only after the restart.
    let (_dir, config) = folder();
    let backend = joined_backend(&config).await;
    let info = address(&backend).await;
    fund(&info.payload, DEPOSIT_SAT);
    drop(backend);
    mine(12);

    let backend = reopened_after_drop(&config).await;
    let events = backend.subscribe();
    let row = mine_until_received(&backend, &events).await;
    assert!(row.amount < Amount::from_sat(DEPOSIT_SAT), "{row:?}");
    wait_balance(&backend, |balance| balance > Amount::ZERO).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn lost_client_db_recovers_balance() {
    let (dir, config) = folder();
    let backend = joined_backend(&config).await;
    let id = backend
        .spending_state()
        .await
        .unwrap()
        .federation
        .unwrap()
        .id;
    let events = backend.subscribe();
    pay_backend_with_lnd(&backend, 10_000).await;
    wait_received(&backend, &events).await;
    let balance = wait_balance(&backend, |balance| balance > Amount::ZERO).await;
    backend.shutdown().await.unwrap();
    drop(backend);
    std::fs::remove_dir_all(dir.path().join("federations").join(&id).join("client")).unwrap();

    let backend = RealBackend::open(config).await.unwrap();
    connected_with(&backend, balance).await;

    // Both v1 modules join the client only on the reopen after recovery: an
    // address takes the wallet, and a credited receive takes the mint.
    address(&backend).await;
    // The next notes must not reuse a nonce the lost database had used.
    let events = backend.subscribe();
    pay_backend_with_lnd(&backend, 5_000).await;
    wait_received(&backend, &events).await;
    wait_balance(&backend, |now| now > balance).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn an_empty_client_database_counts_as_lost() {
    let wallet = Wallet::new();
    let id = wallet.port.federation_id_of(&invite()).unwrap();
    let federation_dir = wallet.dir.path().join("federations").join(&id);
    std::fs::create_dir_all(federation_dir.join("client")).unwrap();
    std::fs::set_permissions(&federation_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        wallet.port.open(&id, &wallet.seed).await.err(),
        Some(FedimixError::UnknownItem(id.clone()))
    );
    let client = wallet.port.recover(&invite(), &wallet.seed).await.unwrap();
    assert_eq!(client.balance().await.unwrap(), Amount::ZERO);
    client.shutdown().await.unwrap();
}

const CAME_BACK: &str = "The payment failed and the money came back.";
/// A refund costs the ecash fees of the notes spent and issued.
const REFUND_COST: Amount = Amount { msat: 100_000 };
/// How many fresh invoices a payment to another wallet gets: a gateway can fail.
const PAY_TRIES: u32 = 5;

fn spending(destination: &str, amount: SendAmount) -> SendRequest {
    SendRequest {
        wallet: WalletKind::Spending,
        destination: destination.to_string(),
        amount,
        fee_rate_sat_vb: None,
    }
}

/// A connected backend holding about `sat`, received over Lightning.
async fn funded_backend(config: &CoreConfig, sat: u64) -> Arc<RealBackend> {
    let backend = joined_backend(config).await;
    let events = backend.subscribe();
    pay_backend_with_lnd(&backend, sat).await;
    wait_received(&backend, &events).await;
    wait_balance(&backend, |balance| balance > Amount::ZERO).await;
    backend
}

async fn balance_of(backend: &RealBackend) -> Amount {
    backend.spending_state().await.unwrap().balance
}

async fn pay_preview(backend: &RealBackend, invoice: &str) -> SendPreview {
    let request = spending(invoice, SendAmount::FromInvoice);
    backend.prepare_send(request).await.unwrap()
}

/// The final state of operation `id`, with the row it names.
async fn final_state(events: &EventStream, id: &str) -> (OperationState, Option<String>) {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match events.next().await {
                Some(Event::Operation {
                    id: seen,
                    state,
                    history_id,
                    ..
                }) if seen == id && state.is_final() => return (state, history_id),
                Some(_) => {}
                None => panic!("the events ended"),
            }
        }
    })
    .await
    .expect("the operation reaches a final state")
}

fn came_back() -> OperationState {
    OperationState::Failed {
        error: FedimixError::Internal(CAME_BACK.into()),
    }
}

/// The row of send `id`, once it is final.
async fn final_row(backend: &RealBackend, id: &str) -> HistoryItem {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let found = rows(backend).await.into_iter().find(|row| row.id == id);
            let row = found.expect("a send keeps its row");
            if !matches!(row.status, HistoryStatus::Pending) {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the row reaches a final status")
}

/// Waits until the balance has fallen from `before` by what [`within_shown`]
/// allows of `shown`. It waits because change can still be on its way.
async fn wait_debited(backend: &RealBackend, before: Amount, shown: Amount) {
    let reached = tokio::time::timeout(PATIENCE, async {
        while !within_shown(before.saturating_sub(balance_of(backend).await), shown) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    let now = balance_of(backend).await;
    assert!(
        reached.is_ok(),
        "from {before:?} with {shown:?} shown, and the balance is {now:?}"
    );
}

async fn wait_restored(backend: &RealBackend, before: Amount) {
    wait_balance(backend, |now| now >= before.saturating_sub(REFUND_COST)).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn pay_lightning_end_to_end() {
    let (_payer_dir, payer_config) = folder();
    let payer = funded_backend(&payer_config, 20_000).await;
    let (_payee_dir, payee_config) = folder();
    let payee = joined_backend(&payee_config).await;
    let payee_events = payee.subscribe();
    let events = payer.subscribe();
    let paid = Amount::from_sat(2_000);

    let mut done = false;
    for attempt in 1..=PAY_TRIES {
        let bill = invoice(&payee, paid.as_sat()).await;
        let before = balance_of(&payer).await;
        let preview = pay_preview(&payer, &bill.payload).await;
        assert_eq!(preview.amount, paid);
        assert!(preview.fee > Amount::ZERO);
        let id = payer.confirm_send(preview.id.clone()).await.unwrap();
        assert_eq!(
            payer.confirm_send(preview.id.clone()).await,
            Err(FedimixError::UnknownItem(preview.id.clone()))
        );
        let (state, history_id) = final_state(&events, &id).await;
        assert_eq!(history_id, Some(id.clone()));
        let row = final_row(&payer, &id).await;
        assert_eq!(row.amount, paid);
        assert!(!row.incoming);
        if state == came_back() {
            eprintln!("REFUNDED on attempt {attempt}");
            assert_eq!(row.status, HistoryStatus::Refunded);
            wait_restored(&payer, before).await;
            continue;
        }
        assert_eq!(state, OperationState::Done);
        assert_eq!(row.status, HistoryStatus::Complete);
        let shown = preview.amount.checked_add(preview.fee).unwrap();
        wait_debited(&payer, before, shown).await;
        done = true;
        break;
    }
    assert!(done, "no attempt settled");
    wait_received(&payee, &payee_events).await;
    payee.shutdown().await.unwrap();
    payer.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn expired_invoice_is_refused_at_preview() {
    let reply = sh("$FM_LNCLI addinvoice --amt 1000 --expiry 1").unwrap();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    let expired = reply["payment_request"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let (_dir, config) = folder();
    let backend = joined_backend(&config).await;
    let request = spending(&expired, SendAmount::FromInvoice);
    assert_eq!(
        backend.prepare_send(request).await,
        Err(FedimixError::InvalidDestination(
            "This invoice has expired.".into()
        ))
    );
    backend.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn lnd_invoice_ends_refunded_and_labelled() {
    let (_dir, config) = folder();
    let backend = funded_backend(&config, 20_000).await;
    let events = backend.subscribe();
    let before = balance_of(&backend).await;
    let preview = pay_preview(&backend, &lnd_invoice(2_000)).await;
    let id = backend.confirm_send(preview.id).await.unwrap();

    let (state, history_id) = final_state(&events, &id).await;
    assert_eq!(state, came_back());
    assert_eq!(history_id, Some(id.clone()));
    assert_eq!(
        final_row(&backend, &id).await.status,
        HistoryStatus::Refunded
    );
    wait_restored(&backend, before).await;
    assert!(backend.operations().await.unwrap().is_empty());
    backend.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn paying_own_invoice_is_handled() {
    let (_dir, config) = folder();
    let backend = funded_backend(&config, 20_000).await;
    let events = backend.subscribe();
    let before = balance_of(&backend).await;
    let bill = invoice(&backend, 2_000).await;
    let preview = pay_preview(&backend, &bill.payload).await;
    let id = backend.confirm_send(preview.id).await.unwrap();

    let (state, history_id) = final_state(&events, &id).await;
    assert_eq!(history_id, Some(id.clone()));
    let row = final_row(&backend, &id).await;
    eprintln!("own invoice ended {state:?}, row {:?}", row.status);
    if state == OperationState::Done {
        assert_eq!(row.status, HistoryStatus::Complete);
    } else {
        assert_eq!(state, came_back());
        assert_eq!(row.status, HistoryStatus::Refunded);
        wait_restored(&backend, before).await;
    }
    assert!(backend.operations().await.unwrap().is_empty());
    backend.shutdown().await.unwrap();
}

/// What bitcoind's wallet received at `address`: sats and the transactions.
fn received_at(address: &str) -> (u64, Vec<String>) {
    let reply = btc(&format!(r#"listreceivedbyaddress 0 true true "{address}""#));
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    let Some(entry) = reply.as_array().and_then(|entries| entries.first()) else {
        return (0, Vec::new());
    };
    let sats = (entry["amount"].as_f64().unwrap() * 100_000_000.0).round() as u64;
    let txids = entry["txids"].as_array().unwrap();
    (
        sats,
        txids
            .iter()
            .map(|txid| txid.as_str().unwrap().to_string())
            .collect(),
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn withdraw_end_to_end() {
    let (_dir, config) = folder();
    let backend = funded_backend(&config, 100_000).await;
    let events = backend.subscribe();
    let sent = Amount::from_sat(50_000);
    let recipient = btc("getnewaddress");
    let before = balance_of(&backend).await;
    let preview = backend
        .prepare_send(spending(&recipient, SendAmount::Exact(sent)))
        .await
        .unwrap();
    assert_eq!(preview.amount, sent);
    assert!(preview.fee > Amount::ZERO);
    let id = backend.confirm_send(preview.id.clone()).await.unwrap();

    let (state, history_id) = final_state(&events, &id).await;
    assert_eq!(state, OperationState::Done);
    assert_eq!(history_id, Some(id.clone()));
    let row = final_row(&backend, &id).await;
    assert_eq!(row.status, HistoryStatus::Complete);
    assert_eq!(row.amount, sent);
    let shown = preview.amount.checked_add(preview.fee).unwrap();
    wait_debited(&backend, before, shown).await;

    tokio::time::timeout(PATIENCE, async {
        while received_at(&recipient).1.is_empty() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .expect("the transaction must reach bitcoind");
    let (received, txids) = received_at(&recipient);
    assert_eq!(
        received,
        sent.as_sat(),
        "the recipient gets the exact amount"
    );
    assert_eq!(txids.len(), 1);
    wait_for_transaction(&txids[0]).await;
    backend.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs devimint"]
async fn killed_after_submit_resumes() {
    let (_payer_dir, config) = folder();
    let mut payer = funded_backend(&config, 20_000).await;
    let (_payee_dir, payee_config) = folder();
    let payee = joined_backend(&payee_config).await;
    let payee_events = payee.subscribe();

    let mut done = false;
    for attempt in 1..=PAY_TRIES {
        let bill = invoice(&payee, 2_000).await;
        let before = balance_of(&payer).await;
        halt_sends(&payer, SendHalt::AfterSubmit);
        let preview = pay_preview(&payer, &bill.payload).await;
        let shown = preview.amount.checked_add(preview.fee).unwrap();
        let id = payer.confirm_send(preview.id).await.unwrap();
        // The debit shows that `pay_invoice` got through, and the hook stops it
        // before the operation id is written or a watch starts.
        wait_balance(&payer, |now| now < before).await;
        assert_eq!(payer.operations().await.unwrap().len(), 1);
        drop(payer);

        payer = reopened_after_drop(&config).await;
        let row = final_row(&payer, &id).await;
        assert!(payer.operations().await.unwrap().is_empty());
        wait_online(&payer).await;
        match row.status {
            HistoryStatus::Complete => {
                wait_debited(&payer, before, shown).await;
                done = true;
                break;
            }
            HistoryStatus::Refunded => {
                eprintln!("REFUNDED on attempt {attempt}");
                wait_restored(&payer, before).await;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(done, "no attempt settled");
    let row = wait_received(&payee, &payee_events).await;
    assert!(row.amount <= Amount::from_sat(2_000));
    let received = rows(&payee).await;
    let credited = received
        .iter()
        .filter(|row| row.status == HistoryStatus::Complete);
    assert_eq!(credited.count(), 1, "the payee was paid once: {received:?}");
    payee.shutdown().await.unwrap();
    payer.shutdown().await.unwrap();
}
