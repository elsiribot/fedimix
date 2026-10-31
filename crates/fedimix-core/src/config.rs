//! Bootstrap configuration handed to the real backend.

pub use crate::api::Network;

#[derive(Debug, Clone, PartialEq)]
pub struct CoreConfig {
    /// Folder holding the database, the seed and the lock.
    pub data_dir: String,
    pub network: Network,
}
