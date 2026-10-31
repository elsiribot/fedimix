//! The only code that touches the Fedimint client. The rest of the core talks
//! to [`FederationPort`], so it can be tested with the fake in `fake.rs`.

#[cfg(feature = "testing")]
pub(crate) mod fake;
mod fedimint;
mod onchain;
mod port;

pub use fedimint::FedimintPort;
pub use port::{
    DepositEvent, FederationClient, FederationPort, FederationPreview, LightningInvoice,
    LightningQuote, LightningTerms, WatchUpdate, WithdrawQuote,
};
