use async_trait::async_trait;

use crate::api::*;
use crate::error::Result;
use crate::event::EventStream;

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    fn subscribe(&self) -> EventStream;
    async fn saving_state(&self) -> Result<SavingState>;
    async fn spending_state(&self) -> Result<SpendingState>;
    async fn queue_state(&self) -> Result<QueueState>;
    async fn settings(&self) -> Result<Settings>;
    async fn set_settings(&self, settings: Settings) -> Result<()>;
    async fn suggested_federations(&self) -> Result<Vec<SuggestedFederation>>;
    async fn join_federation(&self, invite: String) -> Result<FederationInfo>;
    async fn federations(&self) -> Result<Vec<FederationInfo>>;
    async fn set_active_federation(&self, id: String) -> Result<()>;
    async fn detect_devices(&self) -> Result<Vec<DeviceInfo>>;
    async fn setup_saving(&self, device_id: String) -> Result<()>;
    async fn receive(&self, request: ReceiveRequest) -> Result<ReceiveInfo>;
    async fn prepare_send(&self, request: SendRequest) -> Result<SendPreview>;
    async fn confirm_send(&self, preview_id: String) -> Result<()>;
    async fn prepare_refill(&self) -> Result<RefillPreview>;
    async fn confirm_refill(&self, preview_id: String) -> Result<()>;
    async fn exit_now(&self, item_id: String) -> Result<()>;
    async fn set_coin_frozen(&self, outpoint: String, frozen: bool) -> Result<()>;
}
