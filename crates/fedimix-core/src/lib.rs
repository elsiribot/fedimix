//! Fedimix core logic, independent of any UI toolkit.

pub mod api;
pub mod backend;
pub mod config;
pub mod demo;
pub mod destination;
pub mod error;
pub mod event;
pub mod handle;
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
