//! A scripted [`DevicePort`], for tests of what sits above it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{DeviceAccount, DevicePort, Signer};
use crate::api::{DeviceInfo, DeviceKind};
use crate::error::{FedimixError, Result};

#[derive(Default)]
struct State {
    devices: Vec<(DeviceInfo, Arc<dyn Signer>)>,
    connects: usize,
}

/// Lists what was added. `connect` hands back the signer added with that id.
#[derive(Default)]
pub struct FakeDevices {
    state: Mutex<State>,
}

impl FakeDevices {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Plugs in a device. `kind` is what the listing says, whatever `signer` is.
    pub fn add(&self, id: &str, name: &str, kind: DeviceKind, signer: Arc<dyn Signer>) {
        let info = DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            kind,
            connected: true,
        };
        self.state().devices.push((info, signer));
    }

    /// Unplugs a device.
    pub fn remove(&self, id: &str) {
        self.state().devices.retain(|(info, _)| info.id != id);
    }

    /// How many `connect` calls were made.
    pub fn connects(&self) -> usize {
        self.state().connects
    }
}

#[async_trait]
impl DevicePort for FakeDevices {
    async fn list(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self
            .state()
            .devices
            .iter()
            .map(|(info, _)| info.clone())
            .collect())
    }

    async fn connect(&self, id: &str, _account: Option<&DeviceAccount>) -> Result<Arc<dyn Signer>> {
        let mut state = self.state();
        state.connects += 1;
        let device = state.devices.iter().find(|(info, _)| info.id == id);
        device
            .map(|(_, signer)| Arc::clone(signer))
            .ok_or_else(|| FedimixError::DeviceNotConnected("hardware wallet".into()))
    }
}
