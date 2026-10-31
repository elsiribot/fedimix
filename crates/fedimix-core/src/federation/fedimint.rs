//! [`FederationPort`] over `fedimint-client`.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use fedimint_client::module::error::TransactionSubmitError;
use fedimint_client::{Client, ClientBuilder, ClientHandleArc, ClientPreview};
use fedimint_connectors::{ConnectorRegistry, PeerStatus};
use fedimint_core::config::ClientConfig;
use fedimint_core::core::OperationId;
use fedimint_core::db::Database;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::AmountUnit;
use fedimint_core::{NumPeers, PeerId};
use fedimint_lnv2_client::{
    LightningClientModule, LightningOperationMeta, ReceiveError as LnReceiveError,
    ReceiveOperationState, SelectGatewayError, SendOperationState, SendPaymentError,
};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use lightning_invoice::{Bolt11Invoice, Currency};
use tokio::sync::{OnceCell, mpsc, watch};
use tokio::task::JoinSet;

use super::onchain::{Mint, OnChain, config_network, follow_deposits, no_on_chain};
use super::port::{
    DepositEvent, FederationClient, FederationPort, FederationPreview, LightningQuote, WatchUpdate,
    WithdrawQuote,
};
use crate::api::{Amount, Network, NoteBucket, SendAmount};
use crate::error::{FedimixError, Result};
use crate::seed::{Seed, open_data_dir};
use crate::store::{Watch, WatchKind};

/// How long a federation gets to answer before it counts as offline. Fedimint's
/// own requests retry for ever.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Joining fetches the config and the API versions, and writes the database.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `shutdown` waits for calls still running to let go of the client.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);
const RELEASE_POLL: Duration = Duration::from_millis(50);
const STREAM_BUFFER: usize = 16;
pub(super) const NO_RECORD: &str = "Fedimix has no record of this operation.";

pub(super) fn failed(reason: &str) -> WatchUpdate {
    WatchUpdate::Failed {
        reason: reason.to_string(),
    }
}

pub(super) fn amount_of(amount: fedimint_core::Amount) -> Amount {
    Amount { msat: amount.msats }
}

fn fedimint_amount(amount: Amount) -> fedimint_core::Amount {
    fedimint_core::Amount::from_msats(amount.msat)
}

pub(super) fn sat_amount(amount: bitcoin::Amount) -> Amount {
    Amount::from_sat(amount.to_sat())
}

pub(super) fn whole_sats(amount: Amount) -> Result<bitcoin::Amount> {
    if !amount.msat.is_multiple_of(1000) {
        return Err(FedimixError::InvalidAmount(
            "On-chain amounts must be whole sats.".into(),
        ));
    }
    Ok(bitcoin::Amount::from_sat(amount.as_sat()))
}

/// Bounds a call that only reads: past the timeout the federation is offline.
pub(super) async fn read<T>(call: impl Future<Output = T>) -> Result<T> {
    tokio::time::timeout(READ_TIMEOUT, call)
        .await
        .map_err(|_| FedimixError::Offline)
}

pub(super) fn quote_error(
    error: &TransactionSubmitError,
    available: Amount,
    needed: Amount,
) -> FedimixError {
    if error.is_insufficient_funds() {
        return FedimixError::InsufficientFunds { available, needed };
    }
    FedimixError::Internal("Fedimix could not work out the federation's fee.".into())
}

pub(super) fn submit_error(
    error: &TransactionSubmitError,
    available: Amount,
    needed: Amount,
) -> FedimixError {
    if error.is_insufficient_funds() {
        return FedimixError::InsufficientFunds { available, needed };
    }
    FedimixError::Internal("The federation did not accept the withdrawal.".into())
}

fn no_lightning() -> FedimixError {
    FedimixError::NotSetUp("This federation does not support Lightning.".into())
}

fn no_gateway(error: &SelectGatewayError) -> FedimixError {
    match error {
        SelectGatewayError::FailedToRequestGateways(_) => FedimixError::Offline,
        _ => FedimixError::Internal("No Lightning gateway is available right now.".into()),
    }
}

fn invoice_receive_error(error: &LnReceiveError) -> FedimixError {
    match error {
        LnReceiveError::SelectGateway(e) => no_gateway(e),
        LnReceiveError::AmountTooSmall => {
            FedimixError::InvalidAmount("That amount is too small to cover the fees.".into())
        }
        _ => FedimixError::Internal("The Lightning gateway did not make an invoice.".into()),
    }
}

fn pay_error(error: &SendPaymentError, available: Amount, needed: Amount) -> FedimixError {
    match error {
        SendPaymentError::InvoiceMissingAmount => no_invoice_amount(),
        SendPaymentError::InvoiceExpired => expired_invoice(),
        SendPaymentError::DuplicatePaymentAttempt(_) => FedimixError::InvalidDestination(
            "This wallet has already tried to pay this invoice.".into(),
        ),
        SendPaymentError::WrongCurrency { .. } => wrong_currency(),
        SendPaymentError::SelectGateway(e) => no_gateway(e),
        SendPaymentError::InsufficientFunds(_) => {
            FedimixError::InsufficientFunds { available, needed }
        }
        _ => FedimixError::Internal("The Lightning gateway did not take the payment.".into()),
    }
}

fn no_invoice_amount() -> FedimixError {
    FedimixError::InvalidAmount("This invoice has no amount, so it cannot be paid.".into())
}

fn expired_invoice() -> FedimixError {
    FedimixError::InvalidDestination("This invoice has expired.".into())
}

fn wrong_currency() -> FedimixError {
    FedimixError::InvalidDestination("This invoice belongs to a different Bitcoin network.".into())
}

fn parse_invoice(invoice: &str) -> Result<Bolt11Invoice> {
    Bolt11Invoice::from_str(invoice).map_err(|_| {
        FedimixError::InvalidDestination("That is not a valid Lightning invoice.".into())
    })
}

fn parse_invite(invite: &str) -> Result<InviteCode> {
    InviteCode::from_str(invite.trim())
        .map_err(|_| FedimixError::InvalidInvite("That is not a valid invite code.".into()))
}

fn build_error(error: &fedimint_client::error::ClientBuildError) -> FedimixError {
    use fedimint_client::error::ClientBuildError as E;
    match error {
        E::SecretMismatch => FedimixError::Internal(
            "This federation's data was made with a different recovery phrase.".into(),
        ),
        E::ConfigDownload(_) => FedimixError::Offline,
        _ => FedimixError::Internal("Fedimix could not open this federation's data.".into()),
    }
}

fn network_of(network: bitcoin::Network) -> Option<Network> {
    match network {
        bitcoin::Network::Bitcoin => Some(Network::Bitcoin),
        bitcoin::Network::Signet => Some(Network::Signet),
        bitcoin::Network::Regtest => Some(Network::Regtest),
        _ => None,
    }
}

fn federation_name(config: &ClientConfig, id: &str) -> String {
    config.global.federation_name().map_or_else(
        || format!("Federation {}", &id[..id.len().min(8)]),
        str::to_string,
    )
}

fn is_online(status: &BTreeMap<PeerId, PeerStatus>, threshold: usize) -> bool {
    let connected = status
        .values()
        .filter(|s| matches!(s, PeerStatus::Connected(_)))
        .count();
    connected >= threshold
}

/// Keeps the first value and each one that differs from the last kept.
#[derive(Default)]
struct Changes<T>(Option<T>);

impl<T: PartialEq + Copy> Changes<T> {
    fn is_new(&mut self, value: T) -> bool {
        let new = self.0 != Some(value);
        self.0 = Some(value);
        new
    }
}

/// Reaches federations. Client databases live under
/// `<data_dir>/federations/<id>/client`.
pub struct FedimintPort {
    data_dir: PathBuf,
    connectors: OnceCell<ConnectorRegistry>,
}

impl FedimintPort {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            connectors: OnceCell::new(),
        }
    }

    /// Built on first use, once, for every client.
    async fn connectors(&self) -> ConnectorRegistry {
        self.connectors
            .get_or_init(|| ConnectorRegistry::build_from_client_defaults().bind())
            .await
            .clone()
    }

    fn federation_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join("federations").join(id)
    }

    async fn download(&self, code: &InviteCode) -> Result<ClientPreview> {
        let connectors = self.connectors().await;
        read(builder().await.preview(connectors, code))
            .await?
            .map_err(|e| {
                use fedimint_api_client::api::ClientConfigDownloadError as E;
                match e {
                    E::FederationIdMismatch { .. } => FedimixError::InvalidInvite(
                        "This invite does not match the federation that answered.".into(),
                    ),
                    _ => FedimixError::Offline,
                }
            })
    }

    async fn open_db(&self, id: &str) -> Result<Database> {
        let dir = self.federation_dir(id);
        open_data_dir(&dir)?;
        let db = fedimint_rocksdb::RocksDb::build(dir.join("client"))
            .open()
            .await
            .map_err(|_| {
                FedimixError::Internal("Fedimix could not open this federation's data.".into())
            })?;
        Ok(db.into())
    }

    async fn open_existing(
        &self,
        id: &str,
        db: Database,
        root: &Seed,
    ) -> Result<Arc<dyn FederationClient>> {
        let connectors = self.connectors().await;
        let opened = builder().await.open(connectors, db, root.root_secret());
        let client = tokio::time::timeout(CONNECT_TIMEOUT, opened)
            .await
            .map_err(|_| FedimixError::Offline)?
            .map_err(|e| build_error(&e))?;
        FedimintClient::start(id.to_string(), Arc::new(client)).await
    }
}

/// Registers every module generation. A kind with no client module here is
/// left out of the client by `ClientBuilder`, not refused.
async fn builder() -> ClientBuilder {
    let mut builder = Client::builder().await;
    builder.with_module(fedimint_lnv2_client::LightningClientInit::default());
    builder.with_module(fedimint_mint_client::MintClientInit);
    builder.with_module(fedimint_mintv2_client::MintClientInit);
    builder.with_module(fedimint_wallet_client::WalletClientInit(None));
    builder.with_module(fedimint_walletv2_client::WalletClientInit);
    builder
}

#[async_trait]
impl FederationPort for FedimintPort {
    async fn preview(&self, invite: &str) -> Result<FederationPreview> {
        let code = parse_invite(invite)?;
        let preview = self.download(&code).await?;
        let config = preview.config();
        let id = config.calculate_federation_id().to_string();
        Ok(FederationPreview {
            name: federation_name(config, &id),
            network: config_network(config).and_then(network_of),
            has_lightning: config
                .modules
                .values()
                .any(|m| *m.kind() == fedimint_lnv2_common::KIND),
            id,
        })
    }

    async fn join(&self, invite: &str, root: &Seed) -> Result<Arc<dyn FederationClient>> {
        let code = parse_invite(invite)?;
        let preview = self.download(&code).await?;
        let id = preview.config().calculate_federation_id().to_string();
        let db = self.open_db(&id).await?;
        // Joining twice with one secret ruins a wallet, and an interrupted
        // join or a lost store row leaves a database behind.
        if Client::is_initialized(&db).await {
            return self.open_existing(&id, db, root).await;
        }
        let joined = preview.join(db, root.root_secret());
        let client = tokio::time::timeout(CONNECT_TIMEOUT, joined)
            .await
            .map_err(|_| FedimixError::Offline)?
            .map_err(|e| build_error(&e))?;
        FedimintClient::start(id, Arc::new(client)).await
    }

    async fn open(&self, id: &str, root: &Seed) -> Result<Arc<dyn FederationClient>> {
        if !self.federation_dir(id).join("client").is_dir() {
            return Err(FedimixError::UnknownItem(id.to_string()));
        }
        let db = self.open_db(id).await?;
        self.open_existing(id, db, root).await
    }
}

/// One running client. Streams run as tasks here, so a consumer's stream holds
/// no reference to the client, and `shutdown` can end them and let go of it.
struct FedimintClient {
    id: String,
    name: String,
    threshold: usize,
    network: Option<bitcoin::Network>,
    client: RwLock<Option<ClientHandleArc>>,
    stop: watch::Sender<bool>,
    tasks: Mutex<JoinSet<()>>,
}

impl FedimintClient {
    async fn start(id: String, client: ClientHandleArc) -> Result<Arc<dyn FederationClient>> {
        let config = client.config().await;
        // Without this nothing connects until the first request, and
        // `connection_status_stream` would keep reporting nobody.
        client.spawn_federation_reconnect();
        Ok(Arc::new(Self {
            name: federation_name(&config, &id),
            threshold: NumPeers::from(config.global.api_endpoints.len()).threshold(),
            network: config_network(&config),
            id,
            client: RwLock::new(Some(client)),
            stop: watch::Sender::new(false),
            tasks: Mutex::new(JoinSet::new()),
        }))
    }

    fn client(&self) -> Result<ClientHandleArc> {
        self.client
            .read()
            .expect("no writer panics")
            .clone()
            .ok_or_else(|| FedimixError::Internal("This federation has been closed.".into()))
    }

    fn network(&self) -> Result<bitcoin::Network> {
        self.network.ok_or_else(no_on_chain)
    }

    /// Runs `body` as a task feeding the returned stream. The stream ends when
    /// `body` returns or on `shutdown`; dropping it stops the task.
    fn stream<T, F, B>(&self, body: B) -> BoxStream<'static, T>
    where
        T: Send + 'static,
        F: Future<Output = ()> + Send + 'static,
        B: FnOnce(ClientHandleArc, mpsc::Sender<T>) -> F,
    {
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        if let Ok(client) = self.client() {
            let mut stop = self.stop.subscribe();
            let closed = tx.clone();
            let body = body(client, tx);
            let mut tasks = self.tasks.lock().expect("no holder panics");
            while tasks.try_join_next().is_some() {}
            tasks.spawn(async move {
                tokio::select! {
                    () = body => {}
                    _ = stop.wait_for(|stopped| *stopped) => {}
                    () = closed.closed() => {}
                }
            });
        }
        stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed()
    }
}

async fn balance_of(client: &Client) -> Result<fedimint_core::Amount> {
    client
        .get_balance_for_btc()
        .await
        .map_err(|_| FedimixError::NotSetUp("This federation has no ecash module.".into()))
}

async fn follow_ln_receive(client: &Client, op: OperationId, tx: &mpsc::Sender<WatchUpdate>) {
    let Ok(ln) = client.get_first_module::<LightningClientModule>() else {
        let _ = tx.send(failed(NO_RECORD)).await;
        return;
    };
    let Ok(states) = ln.subscribe_receive_operation_state_updates(op).await else {
        let _ = tx.send(failed(NO_RECORD)).await;
        return;
    };
    let mut states = states.into_stream();
    while let Some(state) = states.next().await {
        let update = match state {
            ReceiveOperationState::Pending | ReceiveOperationState::Claiming => continue,
            ReceiveOperationState::Claimed => {
                let (amount, fee) = received(client, op).await.unwrap_or_default();
                WatchUpdate::Settled {
                    amount,
                    fee,
                    txid: None,
                }
            }
            ReceiveOperationState::Expired => WatchUpdate::Expired,
            ReceiveOperationState::Failure => failed("The federation did not credit this payment."),
            ReceiveOperationState::Uneconomical => {
                failed("This payment was too small to be worth claiming.")
            }
        };
        let _ = tx.send(update).await;
    }
}

async fn received(client: &Client, op: OperationId) -> Option<(Amount, Amount)> {
    let entry = client.operation_log().get_operation(op).await?;
    match entry.try_meta::<LightningOperationMeta>().ok()? {
        LightningOperationMeta::Receive(meta) => Some((
            amount_of(meta.contract.commitment.amount),
            amount_of(meta.gateway_fee()),
        )),
        _ => None,
    }
}

/// Waits for the change, which is still being issued when the payment succeeds:
/// a settled operation must leave the balance whole.
async fn paid(client: &Client, op: OperationId) -> Option<(Amount, Amount)> {
    let entry = client.operation_log().get_operation(op).await?;
    match entry.try_meta::<LightningOperationMeta>().ok()? {
        LightningOperationMeta::Send(meta) => {
            let fee = meta.gateway_fee();
            let change = meta.change_outpoint_range.into_iter().collect();
            let _ = client
                .await_primary_bitcoin_module_outputs(op, change)
                .await;
            Some((
                amount_of(meta.contract.amount.saturating_sub(fee)),
                amount_of(fee),
            ))
        }
        _ => None,
    }
}

async fn follow_ln_send(client: &Client, op: OperationId, tx: &mpsc::Sender<WatchUpdate>) {
    let Ok(ln) = client.get_first_module::<LightningClientModule>() else {
        let _ = tx.send(failed(NO_RECORD)).await;
        return;
    };
    let Ok(states) = ln.subscribe_send_operation_state_updates(op).await else {
        let _ = tx.send(failed(NO_RECORD)).await;
        return;
    };
    let mut states = states.into_stream();
    while let Some(state) = states.next().await {
        let update = match state {
            SendOperationState::Funding
            | SendOperationState::Funded
            | SendOperationState::Refunding => continue,
            SendOperationState::Success(_) => {
                let (amount, fee) = paid(client, op).await.unwrap_or_default();
                WatchUpdate::Settled {
                    amount,
                    fee,
                    txid: None,
                }
            }
            SendOperationState::Refunded => WatchUpdate::Refunded,
            SendOperationState::Failure => failed("The payment failed."),
        };
        let _ = tx.send(update).await;
    }
}

async fn follow(client: &Client, watch: Watch, tx: mpsc::Sender<WatchUpdate>) {
    let Ok(op) = OperationId::from_str(&watch.id) else {
        let _ = tx.send(failed(NO_RECORD)).await;
        return;
    };
    match watch.kind {
        WatchKind::LnReceive => follow_ln_receive(client, op, &tx).await,
        WatchKind::LnSend => follow_ln_send(client, op, &tx).await,
        WatchKind::Deposit | WatchKind::Withdraw => {
            let Ok(wallet) = OnChain::of(client) else {
                let _ = tx.send(failed(NO_RECORD)).await;
                return;
            };
            if watch.kind == WatchKind::Deposit {
                wallet.follow_deposit(op, &tx).await;
            } else {
                wallet.follow_withdraw(client, op, &tx).await;
            }
        }
    }
}

#[async_trait]
impl FederationClient for FedimintClient {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    async fn balance(&self) -> Result<Amount> {
        Ok(amount_of(balance_of(&*self.client()?).await?))
    }

    fn balance_changes(&self) -> BoxStream<'static, Amount> {
        self.stream(|client, tx| async move {
            let mut changes = client.subscribe_balance_changes(AmountUnit::BITCOIN).await;
            while let Some(balance) = changes.next().await {
                if tx.send(amount_of(balance)).await.is_err() {
                    return;
                }
            }
        })
    }

    fn online_changes(&self) -> BoxStream<'static, bool> {
        let threshold = self.threshold;
        self.stream(move |client, tx| async move {
            let status = client.connection_status_stream();
            let mut status = std::pin::pin!(status);
            let mut changes = Changes::default();
            while let Some(peers) = status.next().await {
                let online = is_online(&peers, threshold);
                if changes.is_new(online) && tx.send(online).await.is_err() {
                    return;
                }
            }
        })
    }

    async fn notes(&self) -> Result<Vec<NoteBucket>> {
        let client = self.client()?;
        Mint::of(&client)?.notes(&client).await
    }

    async fn min_deposit(&self) -> Result<Amount> {
        let client = self.client()?;
        OnChain::of(&client)?.min_deposit().await
    }

    async fn lightning_invoice(
        &self,
        amount: Amount,
        description: &str,
        expiry_secs: u32,
    ) -> Result<(String, String)> {
        let client = self.client()?;
        let ln = client
            .get_first_module::<LightningClientModule>()
            .map_err(|_| no_lightning())?;
        let description = Bolt11InvoiceDescription::Direct(description.to_string());
        let call = ln.receive(
            fedimint_amount(amount),
            expiry_secs,
            description,
            None,
            serde_json::json!({}),
        );
        let (invoice, op) = read(call).await?.map_err(|e| invoice_receive_error(&e))?;
        Ok((invoice.to_string(), op.fmt_full().to_string()))
    }

    async fn deposit_address(&self) -> Result<(String, Option<String>)> {
        let client = self.client()?;
        OnChain::of(&client)?.deposit_address().await
    }

    async fn lightning_quote(&self, invoice: &str) -> Result<LightningQuote> {
        let client = self.client()?;
        let ln = client
            .get_first_module::<LightningClientModule>()
            .map_err(|_| no_lightning())?;
        let parsed = parse_invoice(invoice)?;
        let msat = parsed
            .amount_milli_satoshis()
            .ok_or_else(no_invoice_amount)?;
        if parsed.is_expired() {
            return Err(expired_invoice());
        }
        if let Some(network) = self.network
            && parsed.currency() != Currency::from(network)
        {
            return Err(wrong_currency());
        }
        let (_, routing) = read(ln.select_gateway(Some(parsed.clone())))
            .await?
            .map_err(|e| no_gateway(&e))?;
        let (send_fee, _) = routing.send_parameters(&parsed);
        let contract = send_fee.add_to(msat);
        let available = amount_of(balance_of(&client).await?);
        let quote = read(ln.send_fee_quote(contract))
            .await?
            .map_err(|e| quote_error(&e, available, amount_of(contract)))?;
        let amount = Amount { msat };
        let total = amount_of(contract + quote.total().get_bitcoin());
        Ok(LightningQuote {
            amount,
            fee: total.saturating_sub(amount),
        })
    }

    async fn pay_invoice(&self, invoice: &str) -> Result<String> {
        let client = self.client()?;
        let ln = client
            .get_first_module::<LightningClientModule>()
            .map_err(|_| no_lightning())?;
        let parsed = parse_invoice(invoice)?;
        let needed = Amount {
            msat: parsed.amount_milli_satoshis().unwrap_or(0),
        };
        let available = amount_of(balance_of(&client).await?);
        let op = ln
            .send(parsed, None, serde_json::json!({}))
            .await
            .map_err(|e| pay_error(&e, available, needed))?;
        Ok(op.fmt_full().to_string())
    }

    async fn withdraw_quote(&self, address: &str, amount: SendAmount) -> Result<WithdrawQuote> {
        let client = self.client()?;
        let wallet = OnChain::of(&client)?;
        let balance = balance_of(&client).await?;
        wallet
            .withdraw_quote(address, amount, balance, self.network()?)
            .await
    }

    async fn withdraw(&self, quote: &WithdrawQuote) -> Result<String> {
        let client = self.client()?;
        let wallet = OnChain::of(&client)?;
        let balance = balance_of(&client).await?;
        let op = wallet.withdraw(quote, balance, self.network()?).await?;
        Ok(op.fmt_full().to_string())
    }

    fn watch(&self, watch: Watch) -> BoxStream<'static, WatchUpdate> {
        self.stream(|client, tx| async move { follow(&client, watch, tx).await })
    }

    fn deposits(&self, cursor: u64) -> BoxStream<'static, DepositEvent> {
        self.stream(move |client, tx| async move {
            if matches!(OnChain::of(&client), Ok(OnChain::V2(_))) {
                follow_deposits(&client, cursor, &tx).await;
            }
        })
    }

    async fn shutdown(self: Arc<Self>) -> Result<()> {
        self.stop.send_replace(true);
        let tasks = std::mem::take(&mut *self.tasks.lock().expect("no holder panics"));
        tasks.join_all().await;
        let client = self.client.write().expect("no writer panics").take();
        let Some(mut client) = client else {
            return Ok(());
        };
        // A call still running holds a reference for a moment.
        let deadline = tokio::time::Instant::now() + RELEASE_TIMEOUT;
        loop {
            match Arc::try_unwrap(client) {
                Ok(handle) => {
                    handle.shutdown().await;
                    return Ok(());
                }
                Err(shared) if tokio::time::Instant::now() < deadline => {
                    client = shared;
                    tokio::time::sleep(RELEASE_POLL).await;
                }
                Err(_) => {
                    return Err(FedimixError::Internal(
                        "A call to the federation did not finish in time, so its data was not closed cleanly.".into(),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fedimint_connectors::Connectivity;

    fn peers(connected: u16, total: u16) -> BTreeMap<PeerId, PeerStatus> {
        (0..total)
            .map(|i| {
                let status = if i < connected {
                    PeerStatus::Connected(Connectivity::Unknown)
                } else {
                    PeerStatus::Disconnected
                };
                (PeerId::from(i), status)
            })
            .collect()
    }

    #[test]
    fn online_takes_a_threshold_of_peers() {
        assert!(!is_online(&peers(2, 4), 3));
        assert!(is_online(&peers(3, 4), 3));
        assert!(is_online(&peers(4, 4), 3));
    }

    #[test]
    fn changes_keeps_first_and_differences() {
        let mut changes = Changes::default();
        let kept: Vec<bool> = [false, false, true, true, false]
            .into_iter()
            .filter(|v| changes.is_new(*v))
            .collect();
        assert_eq!(kept, [false, true, false]);
    }

    #[test]
    fn on_chain_amounts_are_whole_sats() {
        assert_eq!(
            whole_sats(Amount::from_sat(5)).unwrap(),
            bitcoin::Amount::from_sat(5)
        );
        assert!(whole_sats(Amount { msat: 5_001 }).is_err());
    }

    #[test]
    fn testnet_is_not_a_known_network() {
        assert_eq!(
            network_of(bitcoin::Network::Regtest),
            Some(Network::Regtest)
        );
        assert_eq!(network_of(bitcoin::Network::Testnet), None);
    }

    #[test]
    fn garbage_invite_is_a_sentence_without_network() {
        assert_eq!(
            parse_invite("fed1nonsense").err(),
            Some(FedimixError::InvalidInvite(
                "That is not a valid invite code.".into()
            ))
        );
    }
}
