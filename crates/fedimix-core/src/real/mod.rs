//! The production [`Backend`]: settings, federations and history from the
//! store, Fedimint behind the [`FederationPort`].
//!
//! No read here awaits the network: reads answer from the store and from what
//! the supervisors in [`events`] keep up to date. The `store` and `state` locks
//! never nest, and neither is held across an await.

mod events;
mod receive;
mod refill;
mod saving;
mod saving_send;
mod send;
mod spending;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use self::events::{Events, left_open, supervise};
#[cfg(feature = "testing")]
pub use self::saving::sync_saving_now;
use self::saving::{ChainTarget, SavingLive, supervise_saving};
use self::saving_send::{HeldSaving, resume_signed};
use self::send::{HeldSend, Submitting};
#[cfg(feature = "testing")]
pub use self::send::{SendHalt, halt_sends};
use crate::api::*;
use crate::backend::Backend;
use crate::config::CoreConfig;
use crate::error::{FedimixError, Result};
use crate::event::{Event, EventStream};
use crate::federation::{FederationClient, FederationPort, FederationPreview, FedimintPort};
use crate::saving::{ChainClient, DevicePort, HwiPort, SavingWallet, connect_chain};
use crate::seed::{DataDirLock, Seed};
use crate::store::{FederationRow, SignedState, Store, Watch};

const DB_FILE: &str = "fedimix.db";
const SHUTTING_DOWN: &str = "Fedimix is shutting down.";
const SAVING_NOT_SET_UP: &str = "Saving arrives in a later version.";
const PROXY_SET: &str =
    "With a proxy set, Fedimix stays offline. Clear the proxy in Settings to connect.";
const SATS_SUPPLY: u64 = 2_100_000_000_000_000;

/// The time in Unix seconds. Injected so tests are deterministic.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// Makes the chain client for a server and a proxy. Injected so tests need no
/// server.
pub type ChainFactory =
    Arc<dyn Fn(&ChainBackend, Option<&str>) -> Result<Arc<dyn ChainClient>> + Send + Sync>;

/// Whether federation connections may exist. Every task and join follows this,
/// so one change reaches them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    /// A proxy is set, and connections cannot use one yet: traffic must not go
    /// direct instead.
    Proxy,
    Stopped,
}

impl Gate {
    fn of(settings: &Settings) -> Self {
        if settings.socks5_proxy.is_some() {
            Self::Proxy
        } else {
            Self::Open
        }
    }
}

/// What is known of one federation only while this process runs.
#[derive(Default)]
struct Live {
    client: Option<Arc<dyn FederationClient>>,
    online: bool,
    notes: Vec<NoteBucket>,
    /// Why the last `open` failed, until one works.
    last_open_error: Option<FedimixError>,
    /// Ends the supervisor's backoff: a join wants it to try now.
    wake: Arc<Notify>,
    /// Reaches the connection's watches with one that started after them.
    new_watches: Option<mpsc::UnboundedSender<Watch>>,
    /// The client's data is being rebuilt from the federation.
    recovering: bool,
}

/// A preview is consumed under this lock, so it pays at most once.
#[derive(Default)]
struct State {
    live: HashMap<String, Live>,
    previews: HashMap<String, HeldSend>,
    saving_previews: HashMap<String, HeldSaving>,
}

enum Reached {
    Known(String),
    Joined(FederationPreview, Arc<dyn FederationClient>),
    /// New to the store, but the seed is older than the store, so it may have
    /// been used there. The supervisor opens or recovers it.
    Unjoined(FederationPreview),
}

/// A panic can happen with a lock held. The data stays valid, as nothing
/// updates it in several steps, so one task's panic must not spread.
fn held<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn system_clock() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What to tell the user about a failed `open`. Waiting cures `Offline`, so
/// that is no problem; every other error keeps failing until they act.
fn problem_of(error: &FedimixError) -> Option<String> {
    match error {
        FedimixError::Offline => None,
        FedimixError::Internal(sentence) | FedimixError::NotSetUp(sentence) => {
            Some(sentence.clone())
        }
        other => Some(other.to_string()),
    }
}

fn saving_not_set_up() -> FedimixError {
    FedimixError::NotSetUp(SAVING_NOT_SET_UP.into())
}

/// The ranges the Settings page offers.
fn check_ranges(settings: &Settings) -> Result<()> {
    let refuse = |sentence: &str| Err(FedimixError::InvalidAmount(sentence.into()));
    if settings.spending_target > Amount::from_sat(SATS_SUPPLY) {
        return refuse("The spending target is more than all the bitcoin there is.");
    }
    if !(1..=1000).contains(&settings.anonymity_target) {
        return refuse("The anonymity target must be between 1 and 1000.");
    }
    if settings.min_time_in_pool_secs > 720 * 3600 {
        return refuse("The minimum time in the pool cannot exceed 720 hours.");
    }
    if settings.online_grace_secs > 120 * 60 {
        return refuse("The grace period cannot exceed 120 minutes.");
    }
    if settings.fee_guard_percent > 100 {
        return refuse("The fee guard cannot exceed 100%.");
    }
    if settings.cover_budget_sat_per_month > 10_000_000 {
        return refuse("The cover budget cannot exceed 10,000,000 sat per month.");
    }
    Ok(())
}

struct Core {
    config: CoreConfig,
    port: Arc<dyn FederationPort>,
    chain_factory: ChainFactory,
    devices: Arc<dyn DevicePort>,
    seed: Seed,
    /// `false` when the seed is older than the store: the store was lost, so
    /// this seed may have been used at a federation the store does not list.
    seed_born_with_store: bool,
    clock: Clock,
    store: Mutex<Store>,
    state: Mutex<State>,
    saving: Mutex<SavingLive>,
    /// Held through a setup, which can wait for the user at the device.
    saving_setup: tokio::sync::Mutex<()>,
    /// Ends the Saving sync task's wait between syncs.
    saving_wake: Notify,
    /// A Send review opened, so the device is worth looking for now.
    review_opened: Notify,
    /// The chain server and proxy in force, so a change reaches the sync task.
    chain: watch::Sender<ChainTarget>,
    /// Sends between their row and the end of `pay_invoice` or `withdraw`. A
    /// watch channel, so `shutdown` can wait for it to empty.
    submitting: watch::Sender<Submitting>,
    #[cfg(feature = "testing")]
    halt: Mutex<Option<SendHalt>>,
    events: Events,
    gate: watch::Sender<Gate>,
    /// One per federation. Held through a join, so joins run one at a time, and
    /// through shutdown, which removes a handle only once its task has ended:
    /// a call that is cancelled leaves the rest to the next one.
    supervisors: tokio::sync::Mutex<Vec<JoinHandle<Result<()>>>>,
    lock: Mutex<Option<DataDirLock>>,
}

impl Core {
    fn store(&self) -> MutexGuard<'_, Store> {
        held(&self.store)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        held(&self.state)
    }

    fn is_active(&self, id: &str) -> Result<bool> {
        Ok(self.store().active_federation()?.as_deref() == Some(id))
    }

    fn ensure_open(&self) -> Result<()> {
        match *self.gate.borrow() {
            Gate::Open => Ok(()),
            Gate::Proxy => Err(FedimixError::ProxyUnsupported),
            Gate::Stopped => Err(FedimixError::Internal(SHUTTING_DOWN.into())),
        }
    }

    fn ensure_running(&self) -> Result<()> {
        match self.ensure_open() {
            Err(FedimixError::ProxyUnsupported) => Ok(()),
            other => other,
        }
    }

    fn info(&self, row: &FederationRow) -> FederationInfo {
        let gate = *self.gate.borrow();
        let open = gate == Gate::Open;
        let (online, recovering, open_error) = self
            .state()
            .live
            .get(&row.id)
            .map_or((false, false, None), |live| {
                (live.online, live.recovering, live.last_open_error.clone())
            });
        FederationInfo {
            id: row.id.clone(),
            name: row.name.clone(),
            online: open && online,
            anonymity_estimate: 0,
            balance: Amount {
                msat: row.balance_msat,
            },
            // Nothing runs behind a closed gate, so say offline instead.
            recovering: open && recovering,
            problem: if gate == Gate::Proxy {
                Some(PROXY_SET.to_string())
            } else {
                open_error.filter(|_| open).and_then(|e| problem_of(&e))
            },
            has_lightning: row.has_lightning,
            invite: row.invite.clone(),
        }
    }

    fn federation(&self, id: &str) -> Result<FederationInfo> {
        let rows = self.store().federations()?;
        let row = rows.iter().find(|row| row.id == id);
        let row = row.ok_or_else(|| FedimixError::UnknownItem(id.to_string()))?;
        Ok(self.info(row))
    }

    /// Tells the supervisor of a known federation that failed to open, for a
    /// reason that waiting will not cure, to try now. It is the supervisor that
    /// recovers a client database that is gone.
    fn retry_open(&self, id: &str) {
        if let Some(live) = self.state().live.get(id)
            && live.client.is_none()
            && !matches!(live.last_open_error, None | Some(FedimixError::Offline))
        {
            live.wake.notify_one();
        }
    }

    /// Preview, network check, join; a known federation needs none of them.
    /// Nothing but port calls, so it can be dropped halfway.
    async fn reach(&self, invite: &str) -> Result<Reached> {
        let id = self.port.federation_id_of(invite)?;
        if self.store().federations()?.iter().any(|row| row.id == id) {
            return Ok(Reached::Known(id));
        }
        let preview = self.port.preview(invite).await?;
        match preview.network {
            Some(network) if network == self.config.network => {}
            Some(_) => {
                return Err(FedimixError::InvalidInvite(
                    "This federation runs on a different Bitcoin network.".into(),
                ));
            }
            None => {
                return Err(FedimixError::InvalidInvite(
                    "Fedimix cannot tell which network this federation runs on.".into(),
                ));
            }
        }
        if !self.seed_born_with_store {
            return Ok(Reached::Unjoined(preview));
        }
        let client = self.port.join(invite, &self.seed).await?;
        Ok(Reached::Joined(preview, client))
    }

    fn new_row(&self, preview: FederationPreview, invite: &str, balance: Amount) -> FederationRow {
        let FederationPreview {
            id,
            name,
            has_lightning,
            ..
        } = preview;
        FederationRow {
            id,
            name,
            invite: invite.to_string(),
            online_at: Some((self.clock)()),
            balance_msat: balance.msat,
            joined_session: None,
            has_lightning,
        }
    }

    async fn join(self: &Arc<Self>, invite: &str) -> Result<FederationInfo> {
        self.ensure_open()?;
        let mut supervisors = self.supervisors.lock().await;
        self.ensure_open()?;
        let mut gate = self.gate.subscribe();
        let reached = tokio::select! {
            reached = self.reach(invite) => reached?,
            () = left_open(&mut gate) => {
                self.ensure_open()?;
                return Err(FedimixError::Offline);
            }
        };
        let (preview, client) = match reached {
            Reached::Known(id) => {
                self.retry_open(&id);
                return self.federation(&id);
            }
            Reached::Unjoined(preview) => {
                let row = self.new_row(preview, invite, Amount::ZERO);
                self.store().add_federation(&row)?;
                supervisors.push(tokio::spawn(supervise(Arc::clone(self), row.id.clone())));
                self.events.emit(&Event::FederationChanged);
                return Ok(self.info(&row));
            }
            Reached::Joined(preview, client) => (preview, client),
        };
        // A client database from an earlier join can already hold money.
        let balance = client.balance().await.unwrap_or(Amount::ZERO);
        let id = client.id();
        let joined_session = client
            .session_count()
            .await
            .inspect_err(|e| eprintln!("fedimix: reading the session count of {id}: {e}"))
            .ok();
        let row = FederationRow {
            id: id.clone(),
            joined_session,
            ..self.new_row(preview, invite, balance)
        };
        let added = self.store().add_federation(&row);
        if let Err(e) = added {
            let _ = client.shutdown().await;
            return Err(e);
        }
        self.state().live.insert(
            id.clone(),
            Live {
                client: Some(client),
                ..Live::default()
            },
        );
        supervisors.push(tokio::spawn(supervise(Arc::clone(self), id)));
        self.events.emit(&Event::FederationChanged);
        Ok(self.info(&row))
    }

    fn set_settings(&self, settings: &Settings) -> Result<()> {
        check_ranges(settings)?;
        self.ensure_running()?;
        // Under the store's lock, so the gate ends up matching the last write.
        let store = self.store();
        let shown = store.settings()?.recovery_words_shown;
        store.set_settings(&Settings {
            recovery_words_shown: settings.recovery_words_shown || shown,
            ..settings.clone()
        })?;
        // As stored, which is what a restart would read.
        let target = ChainTarget::of(&store.settings()?);
        self.chain.send_if_modified(|chain| {
            let changed = *chain != target;
            *chain = target;
            changed
        });
        self.gate.send_if_modified(|gate| {
            let next = if *gate == Gate::Stopped {
                Gate::Stopped
            } else {
                Gate::of(settings)
            };
            let changed = next != *gate;
            *gate = next;
            changed
        });
        drop(store);
        self.events.emit(&Event::SettingsChanged);
        Ok(())
    }

    /// Stops every task and closes every client, then lets go of the folder.
    /// A failing client does not stop the rest; the first error is returned.
    async fn shutdown(&self) -> Result<()> {
        self.gate.send_replace(Gate::Stopped);
        // A join gives up at the gate, or has handed its client to a task.
        let mut supervisors = self.supervisors.lock().await;
        let mut outcome = Ok(());
        while let Some(task) = supervisors.last_mut() {
            let closed = task.await.unwrap_or_else(|e| {
                Err(FedimixError::Internal(format!(
                    "A background task stopped: {e}."
                )))
            });
            supervisors.pop();
            outcome = outcome.and(closed);
        }
        // A submission writes what became of it: the folder stays held for that.
        if self.submissions_ended().await {
            drop(held(&self.lock).take());
        } else {
            outcome = outcome.and(Err(FedimixError::Internal(
                "A payment was still being submitted when Fedimix closed.".into(),
            )));
        }
        self.events.close();
        outcome
    }
}

/// Whether `federations/` holds a client database. The port creates
/// `federations/<id>` only after a seed exists, so none can be here by chance.
/// A folder that cannot be read is an error: guessing "no data" would skip the
/// guard on the very folder it protects.
fn has_federation_data(dir: &Path) -> Result<bool> {
    let path = dir.join("federations");
    match std::fs::read_dir(&path) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(FedimixError::Internal(format!(
            "Fedimix cannot read {} ({e}), so it cannot tell whether this folder holds a wallet. Fix the folder's permissions, or choose another data folder.",
            path.display()
        ))),
    }
}

pub struct RealBackend {
    core: Arc<Core>,
}

impl RealBackend {
    /// Reads the data folder and returns. Federations connect in the
    /// background, so this works with no network. Needs a tokio runtime.
    pub async fn open(config: CoreConfig) -> Result<Arc<RealBackend>> {
        let dir = Path::new(&config.data_dir);
        let port = Arc::new(FedimintPort::new(dir));
        let devices = Arc::new(HwiPort::new(dir, config.network));
        let chain_factory =
            Arc::new(|backend: &ChainBackend, proxy: Option<&str>| connect_chain(backend, proxy));
        Self::build(config, port, chain_factory, devices, Arc::new(system_clock)).await
    }

    /// [`Self::open`] over the given seams.
    #[cfg(feature = "testing")]
    pub async fn with_ports(
        config: CoreConfig,
        port: Arc<dyn FederationPort>,
        chain_factory: ChainFactory,
        devices: Arc<dyn DevicePort>,
    ) -> Result<Arc<RealBackend>> {
        Self::build(config, port, chain_factory, devices, Arc::new(system_clock)).await
    }

    pub(crate) async fn build(
        config: CoreConfig,
        port: Arc<dyn FederationPort>,
        chain_factory: ChainFactory,
        devices: Arc<dyn DevicePort>,
        clock: Clock,
    ) -> Result<Arc<RealBackend>> {
        let dir = PathBuf::from(&config.data_dir);
        // The lock comes first: it decides who may touch the seed and the store.
        let lock = DataDirLock::acquire(&dir)?;
        let seed_is_new = !Seed::exists(&dir, &lock);
        let db = dir.join(DB_FILE);
        // Before `load_or_create`: a new seed cannot open what the old one made.
        if seed_is_new
            && (has_federation_data(&dir)?
                || (db.exists() && Store::open(&db)?.holds_a_wallet()?))
        {
            return Err(Seed::missing(&dir));
        }
        let seed = Seed::load_or_create(&dir, &lock)?;
        let store = Store::open(&db)?;
        store.bind_network(config.network)?;
        let seed_born_with_store = store.seed_born_with_store(seed_is_new)?;
        let settings = store.settings()?;
        let gate = Gate::of(&settings);
        let chain = ChainTarget::of(&settings);
        let rows = store.federations()?;
        let account = store.device_account()?;
        let saving = SavingLive::opened(account.as_ref(), SavingWallet::open(&db, config.network));
        let saving_wallet = saving.wallet();
        let core = Arc::new(Core {
            config,
            port,
            chain_factory,
            devices,
            seed,
            seed_born_with_store,
            clock,
            store: Mutex::new(store),
            state: Mutex::default(),
            saving: Mutex::new(saving),
            saving_setup: tokio::sync::Mutex::default(),
            saving_wake: Notify::new(),
            review_opened: Notify::new(),
            chain: watch::Sender::new(chain),
            submitting: watch::Sender::default(),
            #[cfg(feature = "testing")]
            halt: Mutex::default(),
            events: Events::default(),
            gate: watch::Sender::new(gate),
            supervisors: tokio::sync::Mutex::default(),
            lock: Mutex::new(Some(lock)),
        });
        let mut supervisors: Vec<_> = rows
            .into_iter()
            .map(|row| tokio::spawn(supervise(Arc::clone(&core), row.id)))
            .collect();
        if let Some(wallet) = saving_wallet {
            let signed = core.store().open_signed()?;
            let unsettled = signed
                .into_iter()
                .filter(|row| row.state == SignedState::Signed);
            for row in unsettled {
                let resume = resume_signed(Arc::clone(&core), Arc::clone(&wallet), row);
                supervisors.push(tokio::spawn(resume));
            }
            supervisors.push(tokio::spawn(supervise_saving(Arc::clone(&core), wallet)));
        }
        *core.supervisors.lock().await = supervisors;
        Ok(Arc::new(RealBackend { core }))
    }
}

/// Tasks hold `Core`, so they must be told: without a `shutdown` call they
/// would keep the clients and the lock for the life of the process.
impl Drop for RealBackend {
    fn drop(&mut self) {
        self.core.gate.send_replace(Gate::Stopped);
    }
}

#[async_trait]
impl Backend for RealBackend {
    fn subscribe(&self) -> EventStream {
        self.core.events.subscribe()
    }

    async fn saving_state(&self) -> Result<SavingState> {
        self.core.saving_state()
    }

    async fn spending_state(&self) -> Result<SpendingState> {
        let core = &self.core;
        let saving_ready = core.saving().is_set_up();
        let (rows, active, target) = {
            let store = core.store();
            (
                store.federations()?,
                store.active_federation()?,
                store.settings()?.spending_target,
            )
        };
        let total_balance = rows.iter().fold(Amount::ZERO, |total, row| Amount {
            msat: total.msat.saturating_add(row.balance_msat),
        });
        let Some(row) = rows.iter().find(|row| Some(&row.id) == active.as_ref()) else {
            return Ok(spending::spending_state(
                None,
                total_balance,
                target,
                saving_ready,
                Vec::new(),
                Vec::new(),
            ));
        };
        let federation = core.info(row);
        let notes = match (federation.online, core.state().live.get(&row.id)) {
            (true, Some(live)) => live.notes.clone(),
            _ => Vec::new(),
        };
        let history = core.store().history_page(
            WalletKind::Spending,
            Some(&row.id),
            None,
            SNAPSHOT_HISTORY,
        )?;
        Ok(spending::spending_state(
            Some(federation),
            total_balance,
            target,
            saving_ready,
            notes,
            history,
        ))
    }

    async fn queue_state(&self) -> Result<QueueState> {
        Ok(spending::queue_state())
    }

    async fn settings(&self) -> Result<Settings> {
        self.core.store().settings()
    }

    async fn set_settings(&self, settings: Settings) -> Result<()> {
        self.core.set_settings(&settings)
    }

    async fn suggested_federations(&self) -> Result<Vec<SuggestedFederation>> {
        Ok(Vec::new())
    }

    async fn join_federation(&self, invite: String) -> Result<FederationInfo> {
        self.core.join(&invite).await
    }

    async fn federations(&self) -> Result<Vec<FederationInfo>> {
        let rows = self.core.store().federations()?;
        Ok(rows.iter().map(|row| self.core.info(row)).collect())
    }

    async fn set_active_federation(&self, id: String) -> Result<()> {
        self.core.ensure_running()?;
        self.core.store().set_active(&id)?;
        self.core.events.emit(&Event::FederationChanged);
        Ok(())
    }

    async fn detect_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.core.detect_devices().await
    }

    async fn setup_saving(&self, device_id: String) -> Result<()> {
        self.core.setup_saving(&device_id).await
    }

    async fn receive(&self, request: ReceiveRequest) -> Result<ReceiveInfo> {
        self.core.receive(request).await
    }

    async fn prepare_send(&self, request: SendRequest) -> Result<SendPreview> {
        self.core.prepare_send(request).await
    }

    async fn confirm_send(&self, preview_id: String) -> Result<OperationId> {
        self.core.confirm_send(&preview_id).await
    }

    async fn prepare_refill(&self) -> Result<RefillPreview> {
        self.core.prepare_refill().await
    }

    async fn confirm_refill(&self, preview_id: String) -> Result<OperationId> {
        self.core.confirm_refill(&preview_id)
    }

    async fn prepare_exit(&self, _item_id: String) -> Result<ExitPreview> {
        Err(saving_not_set_up())
    }

    async fn exit_now(&self, _preview_id: String) -> Result<OperationId> {
        Err(saving_not_set_up())
    }

    async fn discard_preview(&self, preview_id: String) -> Result<()> {
        let mut state = self.core.state();
        state.previews.remove(&preview_id);
        state.saving_previews.remove(&preview_id);
        Ok(())
    }

    /// Spending starts at `Broadcasting`, past the point of cancelling. A Saving
    /// send or refill can be cancelled until then.
    async fn cancel_operation(&self, id: OperationId) -> Result<()> {
        if self.core.cancel_saving(&id)? || self.core.store().is_send(&id)? {
            Ok(())
        } else {
            Err(FedimixError::UnknownItem(id))
        }
    }

    async fn set_coin_frozen(&self, outpoint: String, frozen: bool) -> Result<()> {
        self.core.set_coin_frozen(&outpoint, frozen)
    }

    async fn history(
        &self,
        wallet: WalletKind,
        before: Option<String>,
        limit: u32,
    ) -> Result<HistoryPage> {
        let limit = limit as usize;
        if wallet == WalletKind::Saving {
            return self.core.saving_history(before.as_deref(), limit);
        }
        let store = self.core.store();
        let Some(federation) = store.active_federation()? else {
            return Ok(HistoryPage {
                items: Vec::new(),
                more: false,
            });
        };
        let mut items = store.history_page(
            wallet,
            Some(&federation),
            before.as_deref(),
            limit.saturating_add(1),
        )?;
        let more = items.len() > limit;
        items.truncate(limit);
        Ok(HistoryPage { items, more })
    }

    async fn operations(&self) -> Result<Vec<RunningOperation>> {
        self.core.running_sends()
    }

    async fn recovery_words(&self) -> Result<String> {
        Ok(self.core.seed.words())
    }

    async fn shutdown(&self) -> Result<()> {
        self.core.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::federation::fake::FakePort;

    #[tokio::test]
    async fn rows_keep_the_invite_the_balance_and_the_injected_time() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let port = Arc::new(FakePort::default());
        let client = port.add_federation("fed1one", "one", Network::Regtest);
        let time = Arc::new(AtomicU64::new(100));
        let clock = Arc::clone(&time);
        let config = CoreConfig {
            data_dir: dir.path().to_str().unwrap().to_string(),
            network: Network::Regtest,
        };
        let backend = crate::testing::real_backend(
            config,
            port,
            Arc::new(move || clock.load(Ordering::SeqCst)),
        )
        .await
        .unwrap();
        client.set_online(false);
        backend.join_federation("fed1one".into()).await.unwrap();
        time.store(200, Ordering::SeqCst);
        client.set_balance(Amount::from_sat(5));
        client.set_online(true);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let rows = backend.federations().await.unwrap();
                if rows[0].online && rows[0].balance == Amount::from_sat(5) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        backend.shutdown().await.unwrap();

        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        let rows = store.federations().unwrap();
        assert_eq!(rows[0].invite, "fed1one");
        assert_eq!(rows[0].balance_msat, 5_000);
        assert_eq!(rows[0].online_at, Some(200));
    }
}
