//! The boundary around Fedimint: owned types only, amounts in msat.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::anonymity::SessionDeposit;
use crate::api::{Amount, Network, NoteBucket, SendAmount};
use crate::error::Result;
use crate::seed::Seed;
use crate::store::Watch;

/// What an invite says about a federation, before joining it.
#[derive(Debug, Clone, PartialEq)]
pub struct FederationPreview {
    pub id: String,
    pub name: String,
    /// `None` when the federation runs on a network Fedimix does not know, or
    /// has no module that names one.
    pub network: Option<Network>,
    pub has_lightning: bool,
}

/// An invoice made to be paid to this wallet.
#[derive(Debug, Clone, PartialEq)]
pub struct LightningInvoice {
    pub invoice: String,
    pub watch_id: String,
    /// What the gateway and the federation keep of the amount. `None` when it
    /// could not be read; the invoice is good either way.
    pub fee: Option<Amount>,
}

/// What paying an invoice would cost. Taking the quote back to
/// [`FederationClient::pay_invoice`] pays through the gateway and at the terms
/// that were shown, or not at all.
#[derive(Debug, Clone, PartialEq)]
pub struct LightningQuote {
    pub invoice: String,
    /// The amount the invoice asks for.
    pub amount: Amount,
    /// Gateway fee plus federation fee.
    pub fee: Amount,
    pub terms: LightningTerms,
}

/// The gateway a quote chose and what it charges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightningTerms {
    pub gateway: String,
    pub fee_base_msat: u64,
    pub fee_ppm: u64,
    pub expiration_delta: u64,
}

/// A withdrawal that can be submitted as quoted. Taking the quote back to
/// [`FederationClient::withdraw`] submits it with the fee that was shown.
#[derive(Debug, Clone, PartialEq)]
pub struct WithdrawQuote {
    pub address: String,
    pub amount: Amount,
    /// Miner fee plus federation fee.
    pub fee: Amount,
    pub terms: WithdrawTerms,
}

/// The fee terms the federation must see again at submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawTerms {
    V1 {
        sats_per_kvb: u64,
        total_weight: u64,
    },
    V2 {
        miner_fee_sat: u64,
    },
}

/// Progress of a watched operation. Every variant but `Confirming` is final,
/// and the stream ends after it.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchUpdate {
    /// `confirmations` stays 0 until the federation has seen enough of them:
    /// the client reports no count in between. A deposit's `amount` is what
    /// arrived on chain, before the deposit fee, and `txid` names its
    /// transaction where the generation says it.
    Confirming {
        confirmations: u32,
        required: u32,
        amount: Amount,
        txid: Option<String>,
    },
    /// `amount` is what the operation moved: for a receive what reached the
    /// wallet before the ecash fee, for a send what the recipient got. `fee` is
    /// the part on top of that (or taken from it) that the operation names. A
    /// send whose details cannot be read reports both as zero.
    Settled {
        amount: Amount,
        fee: Amount,
        txid: Option<String>,
    },
    Refunded,
    Expired,
    /// The operation took no money: the federation rejected it.
    Failed {
        reason: String,
    },
    /// The outcome cannot be told, so the money may have left: the client has
    /// no record of the operation, or it ended without saying how.
    Unknown {
        reason: String,
    },
}

impl WatchUpdate {
    pub fn is_final(&self) -> bool {
        !matches!(self, Self::Confirming { .. })
    }
}

/// One on-chain deposit seen in the client's event log, where a generation
/// without a deposit operation reports them.
#[derive(Debug, Clone, PartialEq)]
pub struct DepositEvent {
    /// Pass this to [`FederationClient::deposits`] to resume just after this
    /// event.
    pub cursor: u64,
    /// The deposit's outpoint, so the same for every event of one deposit, also
    /// across a retried claim. When the federation names no outpoint it is the
    /// operation id, which a retried claim changes.
    pub id: String,
    /// The deposit net of `fee`, before the ecash fee.
    pub amount: Amount,
    pub fee: Amount,
    pub txid: Option<String>,
    /// The federation accepted the claim and the ecash has been issued.
    pub confirmed: bool,
}

#[async_trait]
pub trait FederationPort: Send + Sync {
    /// The federation id an invite names. Connects to nothing.
    fn federation_id_of(&self, invite: &str) -> Result<String>;
    async fn preview(&self, invite: &str) -> Result<FederationPreview>;
    async fn join(&self, invite: &str, root: &Seed) -> Result<Arc<dyn FederationClient>>;
    /// `UnknownItem` means this device holds no client data for `id`. `Offline`
    /// is the only other error that waiting can cure. Returns once the client
    /// is usable, so this also waits out a recovery that was cut off earlier.
    async fn open(&self, id: &str, root: &Seed) -> Result<Arc<dyn FederationClient>>;
    /// Rebuilds a client whose data was lost, from what the federation holds
    /// and `root`, then returns it ready to use. This takes as long as the
    /// federation's history does, and can be cut off at any await: the next
    /// `open` or `recover` carries on from what was saved. Never a fresh join:
    /// that would derive ecash nonces the seed has already used.
    async fn recover(&self, invite: &str, root: &Seed) -> Result<Arc<dyn FederationClient>>;
}

/// One federation's client. Calls that only read report `Offline` if the
/// federation does not answer in time. `pay_invoice` and `withdraw` are not cut
/// off: an interrupted payment must not look like one that never started.
#[async_trait]
pub trait FederationClient: Send + Sync {
    fn id(&self) -> String;
    fn name(&self) -> String;
    async fn balance(&self) -> Result<Amount>;
    /// The current balance, then each change.
    fn balance_changes(&self) -> BoxStream<'static, Amount>;
    /// Whether a threshold of guardians is connected: the current value, then
    /// each change.
    fn online_changes(&self) -> BoxStream<'static, bool>;
    async fn notes(&self) -> Result<Vec<NoteBucket>>;
    /// How many consensus sessions the federation has finished.
    async fn session_count(&self) -> Result<u64>;
    /// What a completed session accepted: wallet deposit inputs of either
    /// generation and LNv2 incoming contracts. `None` while it is not
    /// complete. An error means the session could not be fetched or decoded;
    /// it is never to be skipped.
    async fn session_deposits(&self, session: u64) -> Result<Option<Vec<SessionDeposit>>>;
    /// A deposit at or below this is never credited. Exceeding it is necessary
    /// but not sufficient: the fee moves with the feerate on v2, and the claim
    /// also pays the ecash fee.
    async fn min_deposit(&self) -> Result<Amount>;
    async fn lightning_invoice(
        &self,
        amount: Amount,
        description: &str,
        expiry_secs: u32,
    ) -> Result<LightningInvoice>;
    /// Returns the address, and a watch id if the generation has one. Without
    /// one, the deposit arrives through [`Self::deposits`].
    async fn deposit_address(&self) -> Result<(String, Option<String>)>;
    async fn lightning_quote(&self, invoice: &str) -> Result<LightningQuote>;
    /// Returns the watch id. `intent` is recorded with the operation, see
    /// [`Self::find_operation`]. `PreviewExpired` means the gateway's terms
    /// changed since the quote, and nothing was paid.
    async fn pay_invoice(&self, quote: &LightningQuote, intent: &str) -> Result<String>;
    async fn withdraw_quote(&self, address: &str, amount: SendAmount) -> Result<WithdrawQuote>;
    /// Returns the watch id. `intent` is recorded with the operation, see
    /// [`Self::find_operation`].
    async fn withdraw(&self, quote: &WithdrawQuote, intent: &str) -> Result<String>;
    /// The watch id of the payment or withdrawal submitted under `intent`, from
    /// the whole operation log. `None` means it was never submitted, so
    /// a caller that recorded the intent first can tell after a crash whether
    /// to resume.
    async fn find_operation(&self, intent: &str) -> Result<Option<String>>;
    /// Follows an operation to its final update and ends. A watch started
    /// after the operation ended yields that final update. A stream can also
    /// end with no final update, on `shutdown` or on a closed client: the
    /// operation has not ended then, so keep its watch and start again.
    fn watch(&self, watch: Watch) -> BoxStream<'static, WatchUpdate>;
    /// The deposits logged after `cursor` (0 is the start), then new ones as
    /// they arrive. Empty on a generation with deposit watches.
    fn deposits(&self, cursor: u64) -> BoxStream<'static, DepositEvent>;
    /// Ends every stream and closes the client's database. A `pay_invoice` or
    /// `withdraw` in flight has no time limit: past a short wait this returns an
    /// error, and `open` of this federation fails until that call ends.
    async fn shutdown(self: Arc<Self>) -> Result<()>;
}
