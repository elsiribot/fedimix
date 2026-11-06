//! The Saving wallet: a watch-only BDK wallet and the seams it works through.
//! Nothing here is public API. Tests reach it through `crate::testing`.

mod chain;
mod devices;
mod electrum_guard;
#[cfg(feature = "testing")]
mod fake_chain;
#[cfg(feature = "testing")]
mod fake_devices;
mod labels;
mod seed_signer;
mod signer;
#[cfg(feature = "testing")]
mod soft_signer;
mod wallet;

pub use chain::{ChainClient, connect_chain, is_invalid, is_refusal, require_server};
pub use devices::{HwiPort, WithSoftware, software_entry};
#[cfg(feature = "testing")]
pub use fake_chain::{BroadcastGoes, FakeChain};
#[cfg(feature = "testing")]
pub use fake_devices::FakeDevices;
pub use labels::{ExitLabel, Label, Labels, SentRecord, Tag};
pub use seed_signer::SeedSigner;
pub use signer::{DevicePort, Signer};
#[cfg(feature = "testing")]
pub use soft_signer::SoftSigner;
pub(crate) use wallet::{ALREADY_SET_UP, RefillPlan, SendPlan, TxPosition, sort_history};
pub use wallet::{DeviceAccount, SavingWallet, SyncSummary};
