//! The Saving wallet: a watch-only BDK wallet and the seams it works through.
//! Nothing here is public API. Tests reach it through `crate::testing`.

mod chain;
mod devices;
#[cfg(feature = "testing")]
mod fake_chain;
#[cfg(feature = "testing")]
mod fake_devices;
mod labels;
mod signer;
#[cfg(feature = "testing")]
mod soft_signer;
mod wallet;

pub use chain::{ChainClient, connect_chain};
pub use devices::HwiPort;
#[cfg(feature = "testing")]
pub use fake_chain::FakeChain;
#[cfg(feature = "testing")]
pub use fake_devices::FakeDevices;
pub use labels::{Label, Labels, SentRecord, Tag};
pub use signer::{DevicePort, Signer};
#[cfg(feature = "testing")]
pub use soft_signer::SoftSigner;
pub use wallet::{DeviceAccount, SavingWallet, SyncSummary};
