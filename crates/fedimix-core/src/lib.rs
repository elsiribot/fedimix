//! Fedimix core logic, independent of any UI toolkit.

pub mod api;
pub mod backend;
pub mod config;
pub mod demo;
pub mod destination;
pub mod error;
pub mod event;
// The only code that touches the Fedimint client.
mod federation;
pub mod handle;
mod real;
mod saving;
mod seed;
mod store;

pub use api::*;
pub use backend::Backend;
pub use config::CoreConfig;
pub use demo::{DemoBackend, DemoScenario, DemoTiming};
pub use destination::{
    Destination, DestinationKind, parse_btc, parse_destination, validate_address,
};
pub use error::{FedimixError, Result};
pub use event::{Event, EventStream, Notice};
pub use handle::Fedimix;
pub use real::RealBackend;

/// For the integration tests only, which cannot reach crate-private items and
/// do not see `cfg(test)` code. The fake and the backend constructor need the
/// `testing` feature, which only this crate's dev-dependency on itself turns
/// on. Not an API: items here may name fedimint, bitcoin or bdk types and
/// change freely, but nothing outside this module may.
#[doc(hidden)]
pub mod testing {
    #[cfg(feature = "testing")]
    pub use crate::federation::fake::{FakeClient, FakePort};
    pub use crate::federation::{
        DepositEvent, FederationClient, FederationPort, FederationPreview, FedimintPort,
        LightningInvoice, LightningQuote, LightningTerms, WatchUpdate, WithdrawQuote,
    };
    #[cfg(feature = "testing")]
    pub use crate::real::{Clock, SendHalt, halt_sends};
    #[cfg(feature = "testing")]
    pub use crate::saving::FakeChain;
    pub use crate::saving::{
        ChainClient, DeviceAccount, Label, Labels, SavingWallet, SentRecord, SyncSummary, Tag,
        connect_chain,
    };
    pub use crate::seed::{DataDirLock, Seed};
    pub use crate::store::{Watch, WatchKind};

    /// [`crate::RealBackend`] over `port`, reading the time from `clock`.
    #[cfg(feature = "testing")]
    pub async fn real_backend(
        config: crate::CoreConfig,
        port: std::sync::Arc<dyn FederationPort>,
        clock: Clock,
    ) -> crate::Result<std::sync::Arc<crate::RealBackend>> {
        crate::RealBackend::with_port(config, port, clock).await
    }
}
