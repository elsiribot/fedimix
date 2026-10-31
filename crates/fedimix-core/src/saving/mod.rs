//! The Saving wallet: a watch-only BDK wallet and the seams it works through.
//! Nothing here is public API. Tests reach it through `crate::testing`.

mod chain;
#[cfg(feature = "testing")]
mod fake_chain;

pub use chain::{ChainClient, connect_chain};
#[cfg(feature = "testing")]
pub use fake_chain::FakeChain;
