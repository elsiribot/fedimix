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
    pub use crate::federation::fake::{FakeClient, FakePort, regtest_address};
    pub use crate::federation::{
        DepositEvent, FederationClient, FederationPort, FederationPreview, FedimintPort,
        LightningInvoice, LightningQuote, LightningTerms, WatchUpdate, WithdrawQuote,
    };
    #[cfg(feature = "testing")]
    pub use crate::real::{ChainFactory, Clock, SendHalt, halt_sends, sync_saving_now};
    #[cfg(feature = "testing")]
    pub use crate::saving::{BroadcastGoes, FakeChain, FakeDevices, SoftSigner};
    pub use crate::saving::{
        ChainClient, DeviceAccount, DevicePort, HwiPort, Label, Labels, SavingWallet, SentRecord,
        Signer, SyncSummary, Tag, connect_chain,
    };
    pub use crate::seed::{DataDirLock, Seed};
    pub use crate::store::{Watch, WatchKind};

    /// The id and name of the device `real_backend_with_test_signer` lists.
    #[cfg(feature = "testing")]
    pub const TEST_SIGNER_ID: &str = "ledger:test";
    #[cfg(feature = "testing")]
    pub const TEST_SIGNER_NAME: &str = "Test Ledger";

    /// The production [`crate::RealBackend`], but with one device: a Ledger
    /// in the listing that signs with `xprv`, or declines when `rejects`, as
    /// the core tests stand in for hardware. For the real UI flows only.
    #[cfg(feature = "testing")]
    pub async fn real_backend_with_test_signer(
        config: crate::CoreConfig,
        xprv: &str,
        rejects: bool,
    ) -> crate::Result<std::sync::Arc<crate::RealBackend>> {
        let signer = SoftSigner::from_xprv(xprv)?;
        let signer = if rejects { signer.rejecting() } else { signer };
        let devices = FakeDevices::new();
        devices.add(
            TEST_SIGNER_ID,
            TEST_SIGNER_NAME,
            crate::DeviceKind::Ledger,
            std::sync::Arc::new(signer),
        );
        let port = std::sync::Arc::new(FedimintPort::new(std::path::Path::new(&config.data_dir)));
        crate::RealBackend::with_ports(
            config,
            port,
            std::sync::Arc::new(|backend: &crate::ChainBackend, proxy: Option<&str>| {
                connect_chain(backend, proxy)
            }),
            std::sync::Arc::new(devices),
        )
        .await
    }

    /// [`crate::RealBackend`] over `port`, reading the time from `clock`, with
    /// the real chain connector and no devices.
    #[cfg(feature = "testing")]
    pub async fn real_backend(
        config: crate::CoreConfig,
        port: std::sync::Arc<dyn FederationPort>,
        clock: Clock,
    ) -> crate::Result<std::sync::Arc<crate::RealBackend>> {
        crate::RealBackend::build(
            config,
            port,
            std::sync::Arc::new(|backend: &crate::ChainBackend, proxy: Option<&str>| {
                connect_chain(backend, proxy)
            }),
            std::sync::Arc::new(FakeDevices::new()),
            clock,
        )
        .await
    }
}
