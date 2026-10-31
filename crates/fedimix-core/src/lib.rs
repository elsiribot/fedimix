//! Fedimix core logic, independent of any UI toolkit.

pub mod api;
pub mod backend;
pub mod config;
pub mod demo;
pub mod destination;
pub mod error;
pub mod event;
pub mod handle;
// The only code that touches the Fedimint client. Receive and send arrive in
// Tasks 7-8 and use the rest of it.
#[allow(dead_code)]
mod federation;
mod real;
mod seed;
// Watches and cursors are for Tasks 7-8.
#[allow(dead_code)]
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
/// on. Not an API: nothing here may name a fedimint or bitcoin type.
#[doc(hidden)]
pub mod testing {
    #[cfg(feature = "testing")]
    pub use crate::federation::fake::{FakeClient, FakePort};
    pub use crate::federation::{
        DepositEvent, FederationClient, FederationPort, FederationPreview, FedimintPort,
        LightningQuote, LightningTerms, WatchUpdate, WithdrawQuote,
    };
    #[cfg(feature = "testing")]
    pub use crate::real::{Clock, SendHalt, halt_sends};
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
