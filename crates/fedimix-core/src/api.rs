//! Plain owned data types exchanged between the UI and the core.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::FedimixError;

/// Amounts from this many sats up display in BTC, smaller ones in sat.
const DISPLAY_BTC_FROM_SAT: u64 = 100_000;
const SATS_PER_BTC: u64 = 100_000_000;

/// Amount in millisatoshis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Amount {
    pub msat: u64,
}

impl Amount {
    pub const ZERO: Amount = Amount { msat: 0 };

    /// Saturates at `u64::MAX` msat. Valid amounts (at most the 21M BTC
    /// supply, 2.1e18 msat) never reach it, but user input must not panic.
    pub fn from_sat(sat: u64) -> Self {
        Amount {
            msat: sat.saturating_mul(1000),
        }
    }

    /// Whole sats, rounding down.
    pub fn as_sat(self) -> u64 {
        self.msat / 1000
    }

    /// Whole sats, rounding up; for fees, which must never read as zero.
    pub fn as_sat_ceil(self) -> u64 {
        self.msat.div_ceil(1000)
    }

    pub fn checked_add(self, rhs: Amount) -> Option<Amount> {
        self.msat.checked_add(rhs.msat).map(|msat| Amount { msat })
    }

    pub fn checked_sub(self, rhs: Amount) -> Option<Amount> {
        self.msat.checked_sub(rhs.msat).map(|msat| Amount { msat })
    }

    pub fn saturating_sub(self, rhs: Amount) -> Amount {
        Amount {
            msat: self.msat.saturating_sub(rhs.msat),
        }
    }
}

/// Human text: `0.0123 BTC` from 0.001 BTC up, otherwise `420 sat`.
impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sat = self.as_sat();
        if sat < DISPLAY_BTC_FROM_SAT {
            return write!(f, "{} sat", self.as_sat_ceil());
        }
        let whole = sat / SATS_PER_BTC;
        let frac = format!("{:08}", sat % SATS_PER_BTC);
        match frac.trim_end_matches('0') {
            "" => write!(f, "{whole} BTC"),
            frac => write!(f, "{whole}.{frac} BTC"),
        }
    }
}

/// Unix seconds.
pub type Timestamp = u64;

/// Most history rows a [`SavingState`] or [`SpendingState`] carries, newest
/// first. Older rows come from `history`.
pub const SNAPSHOT_HISTORY: usize = 50;

/// Identifies a running operation; its progress arrives as
/// [`crate::Event::Operation`].
pub type OperationId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Send,
    Refill,
    Exit,
}

/// An operation that has not reached a final state.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningOperation {
    pub id: OperationId,
    pub kind: OperationKind,
    pub state: OperationState,
}

/// `Done`, `Failed` and `Cancelled` are final. Operations that need no
/// signature start at `Broadcasting`.
#[derive(Debug, Clone, PartialEq)]
pub enum OperationState {
    AwaitingDevice,
    Signing,
    Broadcasting,
    Done,
    Failed { error: FedimixError },
    Cancelled,
}

impl OperationState {
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. } | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletKind {
    Saving,
    Spending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Ledger,
    Trezor,
    Coldcard,
    Jade,
    BitBox,
    /// Test signer; never offered outside tests.
    Software,
}

/// A hardware wallet. The device Saving was set up with stays in
/// `SavingState::device` while unplugged, as `Some` with `connected: false`;
/// `None` means Saving is not set up.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FederationInfo {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub anonymity_estimate: u32,
    /// What this federation holds for the user.
    pub balance: Amount,
    /// Its data is being rebuilt from the federation. Stays true across
    /// failed attempts, which keep retrying, until the client is usable.
    pub recovering: bool,
    /// A sentence on why this federation cannot be reached, when waiting will
    /// not cure it. `None` covers plain offline, which `online` says.
    pub problem: Option<String>,
    /// It runs Lightning, so Spending can pay and receive over it here.
    pub has_lightning: bool,
    /// The invite code it was joined with; the README tells users to keep it.
    pub invite: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HistoryKind {
    FromSpending {
        denomination: Amount,
    },
    Sent {
        change: Option<SentChange>,
    },
    /// Saving paid into Spending: a refill. `amount` is what reached the
    /// federation's address.
    Refill,
    Received {
        unmixed: bool,
    },
    Paid,
    ReceivedLightning,
    ToSaving {
        denomination: Amount,
    },
    Deposit,
    Cover,
}

/// `to_spending == false` means "small change".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SentChange {
    pub amount: Amount,
    pub to_spending: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HistoryStatus {
    Pending,
    Confirming {
        confirmations: u32,
        required: u32,
    },
    Complete,
    Failed,
    /// A payment that failed and came back.
    Refunded,
    /// An invoice nobody paid in time.
    Expired,
    /// A Saving send no server holds now, which may still confirm: a miner can
    /// have it. Never stored; read from the signed transaction.
    Stalled,
    /// A Saving send that cannot confirm: another one spending the same coins
    /// did. `reason` says which, as a sentence.
    Lost {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: String,
    pub kind: HistoryKind,
    pub amount: Amount,
    pub incoming: bool,
    pub status: HistoryStatus,
    pub timestamp: Timestamp,
    /// The on-chain transaction, once there is one.
    #[serde(default)]
    pub txid: Option<String>,
    /// What this cost on top of `amount`, or took from it when incoming.
    #[serde(default)]
    pub fee: Option<Amount>,
    /// The federation's session count when a deposit settled, for the
    /// anonymity count of later phases. `None` if it could not be read.
    #[serde(default)]
    pub session: Option<u64>,
}

/// One page of history, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPage {
    pub items: Vec<HistoryItem>,
    /// Older rows exist beyond this page.
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PrivacyTag {
    Anonymity { estimate: u32 },
    EarlyExit { estimate: u32 },
    Unmixed,
    SmallChange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Coin {
    pub outpoint: String,
    pub amount: Amount,
    pub tag: PrivacyTag,
    pub received_at: Timestamp,
    pub address: String,
    pub source_federation: Option<String>,
    pub frozen: bool,
    /// In a block. `coins` can sum above `balance` while this is false.
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavingState {
    pub device: Option<DeviceInfo>,
    pub balance: Amount,
    pub in_flight: Amount,
    pub coins: Vec<Coin>,
    pub history: Vec<HistoryItem>,
    /// The last sync reached the chain server. While false, the numbers above
    /// are the last ones known.
    pub chain_online: bool,
    /// What the user can do about the chain server, as a sentence. Being
    /// offline is not one: waiting cures that.
    pub problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoteBucket {
    pub denomination: Amount,
    pub count: u32,
}

/// What this backend can do. A front end hides what it cannot, so nothing
/// implies a feature that does not exist yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    /// Exits from Spending to Saving, and the settings that steer them.
    pub scheduler: bool,
    pub cover: bool,
    pub saving: bool,
}

impl Features {
    pub const ALL: Self = Self {
        scheduler: true,
        cover: true,
        saving: true,
    };
    pub const NONE: Self = Self {
        scheduler: false,
        cover: false,
        saving: false,
    };
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpendingState {
    pub federation: Option<FederationInfo>,
    pub balance: Amount,
    /// What every joined federation holds, the active one included.
    pub total_balance: Amount,
    pub target: Amount,
    pub for_spending: Amount,
    pub moving: Amount,
    pub saving_ready: bool,
    pub notes: Vec<NoteBucket>,
    pub history: Vec<HistoryItem>,
    pub features: Features,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueueItemStatus {
    Queued,
    Eligible,
    Scheduled {
        at: Timestamp,
    },
    /// Waiting for a condition; see [`DeferReason`].
    Deferred {
        reason: DeferReason,
    },
    /// Fedimint reports the txid only once the withdrawal has succeeded.
    Withdrawing {
        txid: Option<String>,
    },
    Landed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    /// The withdrawal fee is above the fee guard.
    FeeGuard,
    /// Spending cannot pay the withdrawal fee.
    FeeUnaffordable,
    Offline,
    /// Within the online grace period.
    GracePeriod,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueItem {
    pub id: String,
    pub denomination: Amount,
    pub status: QueueItemStatus,
    pub anonymity_current: u32,
    pub anonymity_attainable: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueState {
    pub total: Amount,
    pub anonymity_estimate: u32,
    pub typical_exit_secs: u64,
    pub waiting_for_saving: bool,
    pub items: Vec<QueueItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendRequest {
    pub wallet: WalletKind,
    /// As entered; the core parses it with [`crate::parse_destination`].
    pub destination: String,
    pub amount: SendAmount,
    /// On-chain fee rate for Saving sends; `None` is the backend's estimate.
    /// Spending ignores it: the federation sets its fee.
    pub fee_rate_sat_vb: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAmount {
    Exact(Amount),
    /// Everything spendable, minus the fee.
    Max,
    /// The amount the destination carries (invoice or BIP21 URI).
    FromInvoice,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendInput {
    pub outpoint_or_note: String,
    pub amount: Amount,
    pub tag: Option<PrivacyTag>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeDestination {
    None,
    Spending { amount: Amount },
    SmallChange { amount: Amount },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendPreview {
    pub id: String,
    pub wallet: WalletKind,
    pub destination: String,
    pub amount: Amount,
    pub fee: Amount,
    /// The id of the federation this pays from, whichever is active by the time
    /// it is confirmed. For Saving, the federation that takes the change, if any.
    pub federation: Option<String>,
    pub inputs: Vec<SendInput>,
    pub change: ChangeDestination,
    /// Sentences the review should show, such as why the balance is more than
    /// can be sent, or why change stays in Saving.
    pub notes: Vec<String>,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RefillPreview {
    pub id: String,
    pub amount: Amount,
    pub fee: Amount,
    pub coins: Vec<Coin>,
    pub overshoot: Amount,
    pub device_name: String,
    pub expires_at: Timestamp,
}

/// What a manual exit of `item_id` costs. Pass `id` to `exit_now`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExitPreview {
    pub id: String,
    pub item_id: String,
    pub fee: Amount,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReceiveMethod {
    Lightning {
        amount: Amount,
        description: String,
        expiry_secs: u32,
    },
    OnChain,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReceiveRequest {
    pub wallet: WalletKind,
    pub method: ReceiveMethod,
}

/// `payload` is the text shown; `uri` is the QR content.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiveInfo {
    pub payload: String,
    pub uri: String,
    /// When an invoice stops being payable; addresses do not expire.
    pub expires_at: Option<Timestamp>,
    /// On-chain deposits below this are not credited. It is a floor, not a
    /// promise: a deposit just above it can still fall short of the fees.
    pub min_deposit: Option<Amount>,
    /// What the gateway and the federation keep of an invoice's amount, when
    /// that could be read.
    pub fee: Option<Amount>,
    /// This address is shown again until a payment to it is seen, so two
    /// payers given it would be linked on chain.
    pub address_repeats: bool,
}

/// The Bitcoin network a data folder belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Bitcoin,
    Signet,
    Regtest,
}

impl Network {
    /// Lowercase, as the data folder stores it and `--network` takes it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Bitcoin => "bitcoin",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }
}

impl Network {
    /// The public Esplora server to start from; regtest has none.
    pub fn default_esplora_url(self) -> &'static str {
        match self {
            Self::Bitcoin => "https://blockstream.info/api",
            Self::Signet => "https://mempool.space/signet/api",
            Self::Regtest => "",
        }
    }
}

impl std::str::FromStr for Network {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "bitcoin" => Ok(Self::Bitcoin),
            "signet" => Ok(Self::Signet),
            "regtest" => Ok(Self::Regtest),
            other => Err(format!(
                "unknown network '{other}' (expected bitcoin, signet, regtest)"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SuggestedFederation {
    pub name: String,
    pub invite: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChainBackend {
    Esplora { url: String },
    Electrum { url: String },
}

/// `serde(default)`: a stored row from an older version gains new fields at
/// their defaults instead of failing to load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub spending_target: Amount,
    pub anonymity_target: u32,
    pub min_time_in_pool_secs: u64,
    pub online_grace_secs: u64,
    pub fee_guard_percent: u32,
    pub cover_budget_sat_per_month: u64,
    pub chain_backend: ChainBackend,
    pub socks5_proxy: Option<String>,
    /// The user has opened the recovery words page. Once set, a write of
    /// `false` does not clear it: a Settings page holds an older copy.
    #[serde(default)]
    pub recovery_words_shown: bool,
}

impl Settings {
    /// The defaults for a folder on `network`. Regtest has no public chain
    /// server, so its URL is empty: unset until the user names one.
    pub fn for_network(network: Network) -> Self {
        Settings {
            chain_backend: ChainBackend::Esplora {
                url: network.default_esplora_url().to_string(),
            },
            ..Settings::default()
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            spending_target: Amount::from_sat(5_000_000),
            anonymity_target: 50,
            min_time_in_pool_secs: 86_400,
            online_grace_secs: 600,
            fee_guard_percent: 2,
            cover_budget_sat_per_month: 20_000,
            chain_backend: ChainBackend::Esplora {
                url: Network::Bitcoin.default_esplora_url().to_string(),
            },
            socks5_proxy: None,
            recovery_words_shown: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_chain_server_follows_network() {
        let url = |network| match Settings::for_network(network).chain_backend {
            ChainBackend::Esplora { url } => url,
            other => panic!("{other:?}"),
        };
        assert_eq!(url(Network::Bitcoin), "https://blockstream.info/api");
        assert_eq!(url(Network::Signet), "https://mempool.space/signet/api");
        // Regtest has no public server: it is unset until the user names one.
        assert_eq!(url(Network::Regtest), "");
    }

    #[test]
    fn amount_sat_roundtrip() {
        assert_eq!(Amount::from_sat(5).msat, 5000);
        assert_eq!(Amount::from_sat(5).as_sat(), 5);
    }

    #[test]
    fn amount_max_supply_fits() {
        let a = Amount::from_sat(2_100_000_000_000_000);
        assert_eq!(a.as_sat(), 2_100_000_000_000_000);
    }

    #[test]
    fn from_sat_saturates() {
        assert_eq!(Amount::from_sat(u64::MAX).msat, u64::MAX);
    }

    #[test]
    fn as_sat_ceil_rounds_up() {
        assert_eq!(Amount { msat: 1 }.as_sat_ceil(), 1);
        assert_eq!(Amount { msat: 1000 }.as_sat_ceil(), 1);
        assert_eq!(Amount { msat: 1001 }.as_sat_ceil(), 2);
        assert_eq!(Amount::ZERO.as_sat_ceil(), 0);
    }

    #[test]
    fn display_is_btc_or_sat() {
        assert_eq!(Amount::from_sat(420).to_string(), "420 sat");
        assert_eq!(Amount { msat: 1 }.to_string(), "1 sat");
        assert_eq!(Amount::ZERO.to_string(), "0 sat");
        assert_eq!(Amount::from_sat(99_999).to_string(), "99999 sat");
        assert_eq!(Amount::from_sat(100_000).to_string(), "0.001 BTC");
        assert_eq!(Amount::from_sat(1_230_000).to_string(), "0.0123 BTC");
        assert_eq!(Amount::from_sat(47_000_420).to_string(), "0.4700042 BTC");
        assert_eq!(Amount::from_sat(100_000_000).to_string(), "1 BTC");
    }

    #[test]
    fn settings_defaults_match_spec() {
        let s = Settings::default();
        assert_eq!(s.spending_target, Amount::from_sat(5_000_000));
        assert_eq!(s.anonymity_target, 50);
        assert_eq!(s.min_time_in_pool_secs, 86_400);
        assert_eq!(s.online_grace_secs, 600);
        assert_eq!(s.fee_guard_percent, 2);
        assert_eq!(s.cover_budget_sat_per_month, 20_000);
        assert!(matches!(s.chain_backend, ChainBackend::Esplora { .. }));
        assert_eq!(s.socks5_proxy, None);
        assert!(!s.recovery_words_shown);
    }

    #[test]
    fn a_stored_row_without_the_new_setting_loads() {
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old.as_object_mut().unwrap().remove("recovery_words_shown");
        let loaded: Settings = serde_json::from_value(old).unwrap();
        assert!(!loaded.recovery_words_shown);
    }

    #[test]
    fn a_stored_history_row_without_the_new_fields_loads() {
        let item = HistoryItem {
            id: "old".into(),
            kind: HistoryKind::Paid,
            amount: Amount::from_sat(1),
            incoming: false,
            status: HistoryStatus::Complete,
            timestamp: 1,
            txid: Some("t".into()),
            fee: Some(Amount::from_sat(1)),
            session: Some(1),
        };
        let mut old = serde_json::to_value(&item).unwrap();
        for field in ["txid", "fee", "session"] {
            old.as_object_mut().unwrap().remove(field);
        }
        let loaded: HistoryItem = serde_json::from_value(old).unwrap();
        assert_eq!(
            (loaded.txid, loaded.fee, loaded.session),
            (None, None, None)
        );
    }

    #[test]
    fn networks_parse_by_the_name_they_are_stored_under() {
        for network in [Network::Bitcoin, Network::Signet, Network::Regtest] {
            assert_eq!(network.name().parse(), Ok(network));
        }
        assert!("testnet".parse::<Network>().is_err());
    }
}
