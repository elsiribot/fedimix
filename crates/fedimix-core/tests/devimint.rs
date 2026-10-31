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
    DataDirLock, DepositEvent, FederationClient, FederationPort, FedimintPort, Seed, Watch,
    WatchKind, WatchUpdate,
};
use fedimix_core::{Amount, Backend, CoreConfig, FedimixError, Network, RealBackend, SendAmount};
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
        spent.as_sat().abs_diff(shown.as_sat()) <= 1,
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
    // Receive is not in `RealBackend` yet, so fund the folder through the port.
    // Joining then finds the client database and opens it.
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
