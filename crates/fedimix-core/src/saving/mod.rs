//! The Saving wallet: a watch-only BDK wallet and the seams it works through.
//! Nothing here is public API. Tests reach it through `crate::testing`.

mod chain;
mod devices;
#[cfg(feature = "testing")]
mod fake_chain;
mod labels;
mod signer;
#[cfg(feature = "testing")]
mod soft_signer;
mod wallet;

pub use chain::{ChainClient, connect_chain};
// Used by the Saving operations from Task 5 on.
#[allow(unused_imports)]
pub use devices::HwiPort;
#[cfg(feature = "testing")]
pub use fake_chain::FakeChain;
pub use labels::{Label, Labels, SentRecord, Tag};
#[allow(unused_imports)]
pub use signer::{DevicePort, Signer};
#[cfg(feature = "testing")]
#[allow(unused_imports)]
pub use soft_signer::SoftSigner;
pub use wallet::{DeviceAccount, SavingWallet, SyncSummary};
