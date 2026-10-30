//! Plain owned data types exchanged between the UI and the core.

/// Amount in millisatoshis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Amount {
    pub msat: u64,
}

impl Amount {
    pub const ZERO: Amount = Amount { msat: 0 };

    pub fn from_sat(sat: u64) -> Self {
        Amount { msat: sat * 1000 }
    }

    /// Whole sats, rounding down.
    pub fn as_sat(self) -> u64 {
        self.msat / 1000
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

/// Unix seconds.
pub type Timestamp = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletKind {
    Saving,
    Spending,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FederationInfo {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub anonymity_estimate: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryKind {
    FromSpending { denomination: Amount },
    Sent { change: Option<SentChange> },
    Received { unmixed: bool },
    Paid,
    ReceivedLightning,
    ToSaving { denomination: Amount },
    Deposit,
    Cover,
}

/// `to_spending == false` means "small change".
#[derive(Debug, Clone, PartialEq)]
pub struct SentChange {
    pub amount: Amount,
    pub to_spending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryStatus {
    Confirming { confirmations: u32, required: u32 },
    Complete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryItem {
    pub id: String,
    pub kind: HistoryKind,
    pub amount: Amount,
    pub incoming: bool,
    pub status: HistoryStatus,
    pub timestamp: Timestamp,
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavingState {
    pub device: Option<DeviceInfo>,
    pub balance: Amount,
    pub in_flight: Amount,
    pub coins: Vec<Coin>,
    pub history: Vec<HistoryItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoteBucket {
    pub denomination: Amount,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpendingState {
    pub federation: Option<FederationInfo>,
    pub balance: Amount,
    pub target: Amount,
    pub for_spending: Amount,
    pub moving: Amount,
    pub saving_ready: bool,
    pub notes: Vec<NoteBucket>,
    pub history: Vec<HistoryItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueueItemStatus {
    Queued,
    Eligible,
    Scheduled { at: Timestamp },
    Withdrawing { txid: String },
    Landed,
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
    pub destination: String,
    pub amount: Option<Amount>,
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
    pub inputs: Vec<SendInput>,
    pub change: ChangeDestination,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RefillPreview {
    pub id: String,
    pub amount: Amount,
    pub fee: Amount,
    pub coins: Vec<Coin>,
    pub overshoot: Amount,
    pub device_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReceiveMethod {
    Lightning { amount: Amount },
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct SuggestedFederation {
    pub name: String,
    pub invite: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChainBackend {
    Esplora { url: String },
    Electrum { url: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub spending_target: Amount,
    pub anonymity_target: u32,
    pub min_time_in_pool_secs: u64,
    pub online_grace_secs: u64,
    pub fee_guard_percent: u32,
    pub cover_budget_sat_per_month: u64,
    pub chain_backend: ChainBackend,
    pub socks5_proxy: Option<String>,
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
                url: "https://blockstream.info/api".to_string(),
            },
            socks5_proxy: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
