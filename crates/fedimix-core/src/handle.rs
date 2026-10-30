use std::sync::Arc;

use crate::api::*;
use crate::backend::Backend;
use crate::error::Result;
use crate::event::EventStream;

/// Cheap-to-clone handle the UI talks to; forwards to a [`Backend`].
#[derive(Clone)]
pub struct Fedimix {
    backend: Arc<dyn Backend>,
}

impl Fedimix {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    pub fn subscribe(&self) -> EventStream {
        self.backend.subscribe()
    }
    pub async fn saving_state(&self) -> Result<SavingState> {
        self.backend.saving_state().await
    }
    pub async fn spending_state(&self) -> Result<SpendingState> {
        self.backend.spending_state().await
    }
    pub async fn queue_state(&self) -> Result<QueueState> {
        self.backend.queue_state().await
    }
    pub async fn settings(&self) -> Result<Settings> {
        self.backend.settings().await
    }
    pub async fn set_settings(&self, settings: Settings) -> Result<()> {
        self.backend.set_settings(settings).await
    }
    pub async fn suggested_federations(&self) -> Result<Vec<SuggestedFederation>> {
        self.backend.suggested_federations().await
    }
    pub async fn join_federation(&self, invite: String) -> Result<FederationInfo> {
        self.backend.join_federation(invite).await
    }
    pub async fn federations(&self) -> Result<Vec<FederationInfo>> {
        self.backend.federations().await
    }
    pub async fn set_active_federation(&self, id: String) -> Result<()> {
        self.backend.set_active_federation(id).await
    }
    pub async fn detect_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.backend.detect_devices().await
    }
    pub async fn setup_saving(&self, device: DeviceInfo) -> Result<()> {
        self.backend.setup_saving(device).await
    }
    pub async fn receive(&self, request: ReceiveRequest) -> Result<ReceiveInfo> {
        self.backend.receive(request).await
    }
    pub async fn prepare_send(&self, request: SendRequest) -> Result<SendPreview> {
        self.backend.prepare_send(request).await
    }
    pub async fn confirm_send(&self, preview_id: String) -> Result<()> {
        self.backend.confirm_send(preview_id).await
    }
    pub async fn prepare_refill(&self) -> Result<RefillPreview> {
        self.backend.prepare_refill().await
    }
    pub async fn confirm_refill(&self, preview_id: String) -> Result<()> {
        self.backend.confirm_refill(preview_id).await
    }
    pub async fn exit_now(&self, item_id: String) -> Result<()> {
        self.backend.exit_now(item_id).await
    }
    pub async fn set_coin_frozen(&self, outpoint: String, frozen: bool) -> Result<()> {
        self.backend.set_coin_frozen(outpoint, frozen).await
    }
}
