use crate::api::Amount;

/// `String` payloads are complete sentences unless noted.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FedimixError {
    #[error("invalid destination: {0}")]
    InvalidDestination(String),
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
    #[error("insufficient funds: available {available}, needed {needed}")]
    InsufficientFunds { available: Amount, needed: Amount },
    #[error("invalid invite: {0}")]
    InvalidInvite(String),
    /// Payload is the id that was not found.
    #[error("unknown item: {0}")]
    UnknownItem(String),
    /// Payload is the device name.
    #[error("device not connected: {0}")]
    DeviceNotConnected(String),
    #[error("offline")]
    Offline,
    /// A proxy is set, and federation connections cannot use one yet.
    #[error(
        "Fedimix cannot reach federations through a proxy yet. Clear the proxy in Settings to connect."
    )]
    ProxyUnsupported,
    #[error("not set up: {0}")]
    NotSetUp(String),
    /// The data folder belongs to the network `folder`; Fedimix was started for
    /// `wanted`. Both are lowercase network names, as `--network` takes them.
    #[error(
        "This data folder is for {folder}, but Fedimix was started for {wanted}. Start it for {folder}, or choose another data folder."
    )]
    WrongNetwork { folder: String, wanted: String },
    #[error("preview expired")]
    PreviewExpired,
    #[error("device rejected the transaction")]
    DeviceRejected,
    #[error("cancelled")]
    Cancelled,
    /// Exits wait out the online grace period (spec section 1).
    #[error("exits wait until the app has been online for a while")]
    GracePeriod,
    /// A chain server judged a transaction and said no. Payload is its reason.
    #[error("The chain server refused this transaction: {0}.")]
    TxRejected(String),
    /// A server refused a send that an earlier attempt may have delivered.
    #[error(
        "No server takes this send now, but it may have reached the network, so it could still confirm, and Fedimix keeps offering it until it does or a later send replaces it. Do not pay it again from Spending: your next Saving send spends the same coins, so only one of the two can confirm."
    )]
    SendInDoubt,
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, FedimixError>;
