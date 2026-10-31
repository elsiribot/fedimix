//! The production [`Backend`]: settings, federations and history from the
//! store, Fedimint behind the [`FederationPort`].
//!
//! No read here awaits the network: reads answer from the store and from what
//! the supervisors in [`events`] keep up to date. The `store` and `state` locks
//! never nest, and neither is held across an await.

mod events;
mod receive;
mod spending;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use self::events::{Events, left_open, supervise};
use crate::api::*;
use crate::backend::Backend;
use crate::config::CoreConfig;
use crate::error::{FedimixError, Result};
use crate::event::{Event, EventStream};
use crate::federation::{FederationClient, FederationPort, FederationPreview, FedimintPort};
use crate::seed::{DataDirLock, Seed};
use crate::store::{FederationRow, Store, Watch};

const DB_FILE: &str = "fedimix.db";
const SAVING_NOT_SET_UP: &str = "Saving arrives in a later version.";
const NOT_AVAILABLE: &str = "Not available yet.";
const SATS_SUPPLY: u64 = 2_100_000_000_000_000;

/// The time in Unix seconds. Injected so tests are deterministic.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

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
}

/// Task 8 consumes its previews under this lock.
#[derive(Default)]
struct State {
    live: HashMap<String, Live>,
}

enum Reached {
    Known(String),
    Joined(FederationPreview, Arc<dyn FederationClient>),
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

fn saving_not_set_up() -> FedimixError {
    FedimixError::NotSetUp(SAVING_NOT_SET_UP.into())
}

fn not_available() -> FedimixError {
    FedimixError::Internal(NOT_AVAILABLE.into())
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
    seed: Seed,
    clock: Clock,
    store: Mutex<Store>,
    state: Mutex<State>,
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
            Gate::Stopped => Err(FedimixError::Internal("Fedimix is shutting down.".into())),
        }
    }

    fn ensure_running(&self) -> Result<()> {
        match self.ensure_open() {
            Err(FedimixError::ProxyUnsupported) => Ok(()),
            other => other,
        }
    }

    fn info(&self, row: &FederationRow) -> FederationInfo {
        let open = *self.gate.borrow() == Gate::Open;
        let online = self.state().live.get(&row.id).is_some_and(|l| l.online);
        FederationInfo {
            id: row.id.clone(),
            name: row.name.clone(),
            online: open && online,
            anonymity_estimate: 0,
            balance: Amount {
                msat: row.balance_msat,
            },
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
        let client = self.port.join(invite, &self.seed).await?;
        Ok(Reached::Joined(preview, client))
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
            Reached::Joined(preview, client) => (preview, client),
        };
        // A client database from an earlier join can already hold money.
        let balance = client.balance().await.unwrap_or(Amount::ZERO);
        let id = client.id();
        let row = FederationRow {
            id: id.clone(),
            name: preview.name,
            invite: invite.to_string(),
            online_at: Some((self.clock)()),
            balance_msat: balance.msat,
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
        store.set_settings(settings)?;
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
        drop(held(&self.lock).take());
        self.events.close();
        outcome
    }
}

pub struct RealBackend {
    core: Arc<Core>,
}

impl RealBackend {
    /// Reads the data folder and returns. Federations connect in the
    /// background, so this works with no network. Needs a tokio runtime.
    pub async fn open(config: CoreConfig) -> Result<Arc<RealBackend>> {
        let port = Arc::new(FedimintPort::new(Path::new(&config.data_dir)));
        Self::with_port(config, port, Arc::new(system_clock)).await
    }

    pub(crate) async fn with_port(
        config: CoreConfig,
        port: Arc<dyn FederationPort>,
        clock: Clock,
    ) -> Result<Arc<RealBackend>> {
        let dir = PathBuf::from(&config.data_dir);
        // The lock comes first: it decides who may touch the seed and the store.
        let lock = DataDirLock::acquire(&dir)?;
        let seed = Seed::load_or_create(&dir, &lock)?;
        let store = Store::open(&dir.join(DB_FILE))?;
        store.bind_network(config.network)?;
        let gate = Gate::of(&store.settings()?);
        let rows = store.federations()?;
        let core = Arc::new(Core {
            config,
            port,
            seed,
            clock,
            store: Mutex::new(store),
            state: Mutex::default(),
            events: Events::default(),
            gate: watch::Sender::new(gate),
            supervisors: tokio::sync::Mutex::default(),
            lock: Mutex::new(Some(lock)),
        });
        let supervisors = rows
            .into_iter()
            .map(|row| tokio::spawn(supervise(Arc::clone(&core), row.id)))
            .collect();
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
        Ok(spending::saving_state(self.core.store().history_page(
            WalletKind::Saving,
            None,
            None,
            SNAPSHOT_HISTORY,
        )?))
    }

    async fn spending_state(&self) -> Result<SpendingState> {
        let core = &self.core;
        let (rows, active, target) = {
            let store = core.store();
            (
                store.federations()?,
                store.active_federation()?,
                store.settings()?.spending_target,
            )
        };
        let Some(row) = rows.iter().find(|row| Some(&row.id) == active.as_ref()) else {
            return Ok(spending::spending_state(
                None,
                target,
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
            target,
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
        Ok(Vec::new())
    }

    async fn setup_saving(&self, _device_id: String) -> Result<()> {
        Err(saving_not_set_up())
    }

    async fn receive(&self, request: ReceiveRequest) -> Result<ReceiveInfo> {
        self.core.receive(request).await
    }

    async fn prepare_send(&self, _request: SendRequest) -> Result<SendPreview> {
        Err(not_available())
    }

    async fn confirm_send(&self, _preview_id: String) -> Result<OperationId> {
        Err(not_available())
    }

    async fn prepare_refill(&self) -> Result<RefillPreview> {
        Err(saving_not_set_up())
    }

    async fn confirm_refill(&self, _preview_id: String) -> Result<OperationId> {
        Err(saving_not_set_up())
    }

    async fn prepare_exit(&self, _item_id: String) -> Result<ExitPreview> {
        Err(saving_not_set_up())
    }

    async fn exit_now(&self, _preview_id: String) -> Result<OperationId> {
        Err(saving_not_set_up())
    }

    async fn discard_preview(&self, _preview_id: String) -> Result<()> {
        Ok(())
    }

    async fn cancel_operation(&self, _id: OperationId) -> Result<()> {
        Err(not_available())
    }

    async fn set_coin_frozen(&self, _outpoint: String, _frozen: bool) -> Result<()> {
        Err(saving_not_set_up())
    }

    async fn history(
        &self,
        wallet: WalletKind,
        before: Option<String>,
        limit: u32,
    ) -> Result<HistoryPage> {
        let store = self.core.store();
        let federation = match wallet {
            WalletKind::Saving => None,
            WalletKind::Spending => match store.active_federation()? {
                Some(id) => Some(id),
                None => {
                    return Ok(HistoryPage {
                        items: Vec::new(),
                        more: false,
                    });
                }
            },
        };
        let limit = limit as usize;
        let mut items = store.history_page(
            wallet,
            federation.as_deref(),
            before.as_deref(),
            limit.saturating_add(1),
        )?;
        let more = items.len() > limit;
        items.truncate(limit);
        Ok(HistoryPage { items, more })
    }

    async fn operations(&self) -> Result<Vec<RunningOperation>> {
        Ok(Vec::new())
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
        let backend =
            RealBackend::with_port(config, port, Arc::new(move || clock.load(Ordering::SeqCst)))
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
