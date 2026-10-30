use crate::api::Amount;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FedimixError {
    #[error("invalid destination: {0}")]
    InvalidDestination(String),
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
    #[error("insufficient funds: available {available:?}, needed {needed:?}")]
    InsufficientFunds { available: Amount, needed: Amount },
    #[error("invalid invite: {0}")]
    InvalidInvite(String),
    #[error("unknown item: {0}")]
    UnknownItem(String),
    #[error("device not connected: {0}")]
    DeviceNotConnected(String),
    #[error("offline")]
    Offline,
    #[error("not set up: {0}")]
    NotSetUp(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, FedimixError>;
