//! Fedimix core logic, independent of any UI toolkit.

pub mod api;
pub mod backend;
pub mod error;
pub mod event;
pub mod handle;

pub use api::*;
pub use backend::Backend;
pub use error::{FedimixError, Result};
pub use event::{Event, EventStream, Notice};
pub use handle::Fedimix;
