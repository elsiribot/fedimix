//! Fedimix core logic, independent of any UI toolkit.

pub mod api;
pub mod backend;
pub mod config;
pub mod demo;
pub mod destination;
pub mod error;
pub mod event;
pub mod handle;
// The only code that touches the Fedimint client; RealBackend arrives in Task 6.
#[allow(dead_code)]
mod federation;
// Persistence for the real backend (Phase 2); nothing calls it until Tasks 5-6.
#[allow(dead_code)]
mod seed;
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

/// For `tests/devimint.rs` only, which cannot reach crate-private items. Not an
/// API: nothing here may name a fedimint or bitcoin type.
#[doc(hidden)]
pub mod testing {
    pub use crate::federation::{
        DepositEvent, FederationClient, FederationPort, FederationPreview, FedimintPort,
        LightningQuote, LightningTerms, WatchUpdate, WithdrawQuote,
    };
    pub use crate::seed::{DataDirLock, Seed};
    pub use crate::store::{Watch, WatchKind};
}
