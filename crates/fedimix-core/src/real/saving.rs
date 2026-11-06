//! Saving on the real backend: setup, reads, addresses and the one task that
//! keeps the watch-only wallet in step with the chain.
//!
//! No read here waits for the chain or a device: they answer from the wallet's
//! tables and from what the sync task and the last device poll left in
//! [`SavingLive`]. The `saving` lock nests with no other, and no lock is held
//! across an await.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use super::saving_send::{SavingOp, held_back_of};
use super::{ChainFactory, Core, DB_FILE, Gate, SHUTTING_DOWN, held, problem_of};
use crate::api::*;
use crate::error::{FedimixError, Result};
use crate::event::Event;
use crate::saving::{
    ALREADY_SET_UP, ChainClient, DeviceAccount, Labels, SavingWallet, Signer, require_server,
    software_entry,
};

/// How often a sync runs with nothing going on.
const SYNC_INTERVAL: Duration = Duration::from_secs(60);
/// How often it runs while money is moving.
const ACTIVE_SYNC_INTERVAL: Duration = Duration::from_secs(10);
/// The longest wait after failures.
const MAX_SYNC_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// How often a device is looked for while a review is open.
pub(super) const DEVICE_POLL: Duration = Duration::from_secs(5);

const SET_UP_FIRST: &str = "Set up Saving first.";
const SCANNING: &str = "Fedimix is still scanning Saving's history.";
const SETUP_STOPPED: &str = "Setting up Saving stopped unexpectedly. Try again.";
const SETTINGS_MOVED: &str = "The network settings changed during setup. Set up Saving again.";
const SYNC_PANICKED: &str =
    "Fedimix could not read the chain server's answer. Check the server in Settings.";
const ON_CHAIN_ONLY: &str = "Saving receives on chain, not over Lightning.";
pub(super) const JADE_PROXY: &str =
    "Jade needs Blockstream's PIN server, which cannot use your proxy. Clear the proxy to use it.";

/// What the chain client is made from. A change makes a new client.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChainTarget {
    pub backend: ChainBackend,
    pub proxy: Option<String>,
}

impl ChainTarget {
    pub(super) fn of(settings: &Settings) -> Self {
        Self {
            backend: settings.chain_backend.clone(),
            proxy: settings.socks5_proxy.clone(),
        }
    }

    pub(super) fn connect(&self, factory: &ChainFactory) -> Result<Arc<dyn ChainClient>> {
        require_server(&self.backend)?;
        factory(&self.backend, self.proxy.as_deref())
    }
}

/// What is known of Saving while this process runs.
#[derive(Default)]
pub(super) struct SavingLive {
    /// `None` until set up, or if the stored wallet could not be opened.
    wallet: Option<Arc<SavingWallet>>,
    device: Option<DeviceInfo>,
    chain_online: bool,
    problem: Option<String>,
    /// The one Saving operation in this process, up to its end.
    pub(super) op: Option<SavingOp>,
    /// Counts operations started, so a preview from before one is stale.
    pub(super) generation: u64,
    /// The last operation, so cancelling one that just ended is no error.
    pub(super) last_op: Option<String>,
}

impl SavingLive {
    /// What `RealBackend` starts from: the stored account, and its wallet
    /// unless that failed to open. A Saving that cannot open must not stop the
    /// money in Spending, so that is a `problem` and not a failed start.
    pub(super) fn opened(
        account: Option<&DeviceAccount>,
        wallet: Result<Option<SavingWallet>>,
    ) -> Self {
        let (wallet, problem) = match wallet {
            Ok(wallet) => (wallet.map(Arc::new), None),
            Err(e) => {
                eprintln!("fedimix: opening the Saving wallet: {e}");
                (None, problem_of(&e))
            }
        };
        Self {
            wallet,
            device: account.map(stored_device),
            chain_online: false,
            problem,
            ..Self::default()
        }
    }

    pub(super) fn wallet(&self) -> Option<Arc<SavingWallet>> {
        self.wallet.clone()
    }

    pub(super) fn is_set_up(&self) -> bool {
        self.device.is_some()
    }
}

/// The device of a stored account, unseen since the start: no id from a listing
/// survives a restart, as it names a USB path. The software wallet needs no
/// listing to be there.
fn stored_device(account: &DeviceAccount) -> DeviceInfo {
    let software = account.kind == DeviceKind::Software;
    DeviceInfo {
        id: if software {
            SOFTWARE_DEVICE_ID.to_string()
        } else {
            account.fingerprint.clone()
        },
        name: account.name.clone(),
        kind: account.kind,
        connected: software,
    }
}

/// `m/84'/<coin>'/0'`: coin 0 on bitcoin, 1 elsewhere.
fn account_path(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "m/84'/0'/0'",
        Network::Signet | Network::Regtest => "m/84'/1'/0'",
    }
}

/// How long to wait after `failures` failed syncs in a row.
fn sync_delay(interval: Duration, failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    if failures == 0 {
        interval
    } else {
        (interval * 2u32.pow(doublings)).min(MAX_SYNC_BACKOFF)
    }
}

/// Returns when Fedimix is stopping.
pub(super) async fn stopping(gate: &mut watch::Receiver<Gate>) {
    let _ = gate.wait_for(|gate| *gate == Gate::Stopped).await;
}

impl Core {
    pub(super) fn saving(&self) -> std::sync::MutexGuard<'_, SavingLive> {
        held(&self.saving)
    }

    pub(super) fn saving_wallet(&self) -> Result<Arc<SavingWallet>> {
        self.saving()
            .wallet()
            .ok_or_else(|| FedimixError::NotSetUp(SET_UP_FIRST.into()))
    }

    /// The wallet, once it has seen what the device used before: until then its
    /// address index and its coins are not to be trusted.
    pub(super) fn scanned_wallet(&self) -> Result<Arc<SavingWallet>> {
        let wallet = self.saving_wallet()?;
        if wallet.first_scan_done() {
            return Ok(wallet);
        }
        let problem = self.saving().problem.clone();
        Err(FedimixError::NotSetUp(
            problem.unwrap_or_else(|| SCANNING.to_string()),
        ))
    }

    /// Answers from memory and the wallet's tables, whatever the chain is doing.
    pub(super) fn saving_state(&self) -> Result<SavingState> {
        let (wallet, device, chain_online, problem) = {
            let live = self.saving();
            (
                live.wallet(),
                live.device.clone(),
                live.chain_online,
                live.problem.clone(),
            )
        };
        let mut state = SavingState {
            device,
            balance: Amount::ZERO,
            in_flight: self.exits_in_flight()?,
            coins: Vec::new(),
            history: Vec::new(),
            chain_online,
            problem,
            wallet_open: wallet.is_some(),
            scanning: wallet.as_ref().is_some_and(|w| !w.first_scan_done()),
            held_back: Amount::ZERO,
        };
        if let Some(wallet) = wallet {
            let labels = Labels::load(&self.store())?;
            state.held_back = held_back_of(&wallet, &labels, &self.locked_outpoints()?);
            let snapshot = wallet.snapshot(&labels);
            state.balance = snapshot.balance;
            state.coins = snapshot.coins;
            self.name_sources(&mut state.coins)?;
            state.history = snapshot.history;
            self.add_signed_rows(&wallet, &labels, &mut state.history)?;
            state.history.truncate(SNAPSHOT_HISTORY);
        }
        Ok(state)
    }

    /// Labels keep a coin's source federation by id; the user knows it by
    /// name. An id no row names stays as it is.
    fn name_sources(&self, coins: &mut [Coin]) -> Result<()> {
        if coins.iter().all(|coin| coin.source_federation.is_none()) {
            return Ok(());
        }
        let names: HashMap<String, String> = self
            .store()
            .federations()?
            .into_iter()
            .map(|row| (row.id, row.name))
            .collect();
        for source in coins
            .iter_mut()
            .filter_map(|coin| coin.source_federation.as_mut())
        {
            if let Some(name) = names.get(source.as_str()) {
                source.clone_from(name);
            }
        }
        Ok(())
    }

    pub(super) fn saving_history(&self, before: Option<&str>, limit: usize) -> Result<HistoryPage> {
        let Some(wallet) = self.saving().wallet() else {
            return Ok(HistoryPage {
                items: Vec::new(),
                more: false,
            });
        };
        let labels = Labels::load(&self.store())?;
        let mut items = wallet.history(&labels);
        self.add_signed_rows(&wallet, &labels, &mut items)?;
        if let Some(before) = before {
            let position = items.iter().position(|item| item.id == before);
            let position = position.ok_or_else(|| FedimixError::UnknownItem(before.to_string()))?;
            items.drain(..=position);
        }
        let more = items.len() > limit;
        items.truncate(limit);
        Ok(HistoryPage { items, more })
    }

    pub(super) fn saving_receive(&self, method: &ReceiveMethod) -> Result<ReceiveInfo> {
        if matches!(method, ReceiveMethod::Lightning { .. }) {
            return Err(FedimixError::NotSetUp(ON_CHAIN_ONLY.into()));
        }
        self.ensure_running()?;
        // Before the first scan, the index would start over on a used device
        // and could hand out an address that was already paid.
        let wallet = self.scanned_wallet()?;
        let address = wallet.next_receive_address()?;
        Ok(ReceiveInfo {
            uri: format!("bitcoin:{address}"),
            payload: address,
            expires_at: None,
            min_deposit: None,
            fee: None,
            address_repeats: false,
        })
    }

    pub(super) fn set_coin_frozen(&self, outpoint: &str, frozen: bool) -> Result<()> {
        let wallet = self.saving_wallet()?;
        if !wallet.has_coin(outpoint) {
            return Err(FedimixError::UnknownItem(outpoint.to_string()));
        }
        {
            let store = self.store();
            Labels::load(&store)?.set_frozen(&store, outpoint, frozen)?;
        }
        self.events.emit(&Event::SavingChanged);
        Ok(())
    }

    /// The devices that can be used now, and why no hardware wallet could be
    /// looked for, if so. With a proxy that leaves Jade out: its PIN step talks
    /// to Blockstream outside any proxy. Doubles as the poll behind
    /// `SavingState::device`.
    pub(super) async fn poll_devices(&self) -> (Vec<DeviceInfo>, Option<FedimixError>) {
        let (found, problem, _) = self.poll_devices_noting_jade().await;
        (found, problem)
    }

    /// [`Self::poll_devices`], and whether a Jade was left out for the proxy.
    async fn poll_devices_noting_jade(&self) -> (Vec<DeviceInfo>, Option<FedimixError>, bool) {
        let (mut found, problem) = self.devices.list().await;
        let listed = found.len();
        if self.chain.borrow().proxy.is_some() {
            found.retain(|device| device.kind != DeviceKind::Jade);
        }
        let jade_hidden = found.len() != listed;
        self.note_devices(&found);
        (found, problem, jade_hidden)
    }

    /// A Jade left out for the proxy is said in `problem`, so the person knows
    /// why the one they plugged in is missing.
    pub(super) async fn detect_devices(&self) -> Result<DeviceList> {
        let (devices, problem, jade_hidden) = self.poll_devices_noting_jade().await;
        let sentences: Vec<String> = problem
            .as_ref()
            .and_then(problem_of)
            .into_iter()
            .chain(jade_hidden.then(|| JADE_PROXY.to_string()))
            .collect();
        Ok(DeviceList {
            devices,
            problem: (!sentences.is_empty()).then(|| sentences.join(" ")),
        })
    }

    /// A listing does not open a device, so a device of the kind is taken to be
    /// the one; connecting checks its fingerprint.
    fn note_devices(&self, found: &[DeviceInfo]) {
        let changed = {
            let mut live = self.saving();
            let Some(device) = live.device.as_mut() else {
                return;
            };
            let seen = found.iter().find(|other| other.kind == device.kind);
            let before = device.clone();
            device.connected = seen.is_some();
            if let Some(seen) = seen {
                device.id.clone_from(&seen.id);
            }
            *device != before
        };
        if changed {
            self.events.emit(&Event::SavingChanged);
        }
    }

    /// Opens a listed device, unless it may not be used as things stand.
    pub(super) async fn open_device(
        &self,
        id: &str,
        account: Option<&DeviceAccount>,
    ) -> Result<(DeviceInfo, Arc<dyn Signer>)> {
        // Always there, and needs no scan of USB.
        if id == SOFTWARE_DEVICE_ID {
            let signer = self.devices.connect(id, account).await?;
            return Ok((software_entry(), signer));
        }
        let listed = self.devices.hardware().await?;
        let device = listed.into_iter().find(|device| device.id == id);
        let device = device
            .ok_or_else(|| FedimixError::DeviceNotConnected("hardware wallet".to_string()))?;
        if device.kind == DeviceKind::Jade && self.chain.borrow().proxy.is_some() {
            return Err(FedimixError::NotSetUp(JADE_PROXY.into()));
        }
        let signer = self.devices.connect(id, account).await?;
        Ok((device, signer))
    }

    /// The device's account at the BIP84 path. Public data only.
    async fn read_account(&self, id: &str) -> Result<(DeviceInfo, DeviceAccount)> {
        let (device, signer) = self.open_device(id, None).await?;
        let path = account_path(self.config.network);
        let account = DeviceAccount {
            kind: device.kind,
            name: device.name.clone(),
            fingerprint: signer.fingerprint().await?,
            xpub: signer.xpub(path).await?,
            path: path.to_string(),
        };
        Ok((device, account))
    }

    pub(super) async fn setup_saving(self: &Arc<Self>, id: &str) -> Result<()> {
        // One at a time, and not `supervisors`: a device can wait on the user
        // for minutes, and joins and shutdown must not wait with it.
        let _setting_up = self.saving_setup.lock().await;
        self.ensure_running()?;
        if self.saving().wallet.is_some() {
            return Err(FedimixError::NotSetUp(ALREADY_SET_UP.into()));
        }
        let mut gate = self.gate.subscribe();
        let mut target = self.chain.subscribe();
        // A new proxy ends the wait: the device was checked against the old one.
        let (device, account) = tokio::select! {
            read = self.read_account(id) => read?,
            () = stopping(&mut gate) => {
                return Err(FedimixError::Internal(SHUTTING_DOWN.into()));
            }
            _ = target.changed() => {
                return Err(FedimixError::NotSetUp(SETTINGS_MOVED.into()));
            }
        };
        // In a task of its own: a caller that goes away must not leave a wallet
        // with no sync, and `supervisors` keeps shutdown out until it is done.
        tokio::spawn(Arc::clone(self).finish_setup(device, account))
            .await
            .map_err(|_| FedimixError::Internal(SETUP_STOPPED.into()))?
    }

    /// Writes the wallet and starts its sync, or neither. Holding `supervisors`
    /// from the gate check on, so the folder stays held through the write.
    async fn finish_setup(
        self: Arc<Self>,
        device: DeviceInfo,
        account: DeviceAccount,
    ) -> Result<()> {
        let mut supervisors = self.supervisors.lock().await;
        self.ensure_running()?;
        let db = std::path::Path::new(&self.config.data_dir).join(DB_FILE);
        let network = self.config.network;
        let created =
            tokio::task::spawn_blocking(move || SavingWallet::create(&db, account, network))
                .await
                .map_err(|_| FedimixError::Internal(SETUP_STOPPED.into()))??;
        let wallet = Arc::new(created);
        {
            let mut live = self.saving();
            live.wallet = Some(Arc::clone(&wallet));
            live.device = Some(device);
            live.problem = None;
        }
        supervisors.push(tokio::spawn(supervise_saving(Arc::clone(&self), wallet)));
        self.events.emit(&Event::SavingChanged);
        Ok(())
    }

    /// 10 s while an operation runs or money is on its way in, exits included.
    fn sync_interval(&self, wallet: &SavingWallet) -> Duration {
        let operating = self
            .running_sends()
            .is_ok_and(|running| !running.is_empty());
        let exiting = self
            .exits_in_flight()
            .is_ok_and(|exiting| exiting > Amount::ZERO);
        if operating || exiting || wallet.in_flight() > Amount::ZERO {
            ACTIVE_SYNC_INTERVAL
        } else {
            SYNC_INTERVAL
        }
    }

    /// Keeps what a sync found. Silent unless something changed.
    fn note_chain(&self, online: bool, problem: Option<String>, wallet_changed: bool) {
        let moved = {
            let mut live = self.saving();
            let moved = live.chain_online != online || live.problem != problem;
            live.chain_online = online;
            live.problem = problem;
            moved
        };
        if moved || wallet_changed {
            self.events.emit(&Event::SavingChanged);
        }
    }
}

/// Keeps `wallet` in step with the chain until Fedimix stops. A failing sync
/// waits longer each time, up to five minutes. A new server or proxy drops the
/// call in flight and starts over at once. Only the wait for the chain can be
/// dropped: `apply` writes to the data folder, so it ends before this does.
pub(super) async fn supervise_saving(core: Arc<Core>, wallet: Arc<SavingWallet>) -> Result<()> {
    // Ends with the gate. It only asks the USB stack: the folder is not its.
    tokio::spawn(poll_during_reviews(Arc::clone(&core)));
    let mut gate = core.gate.subscribe();
    let mut target = core.chain.subscribe();
    'connect: loop {
        let current = target.borrow_and_update().clone();
        let chain = match current.connect(&core.chain_factory) {
            Ok(chain) => {
                core.note_chain(false, None, false);
                chain
            }
            // No client, so only a new setting can help.
            Err(e) => {
                core.note_chain(false, problem_of(&e), false);
                tokio::select! {
                    () = stopping(&mut gate) => return Ok(()),
                    _ = target.changed() => continue 'connect,
                }
            }
        };
        let mut failures = 0;
        loop {
            // A task of its own, as it runs code fed by a remote server: a
            // panic there is a failed sync, not the end of syncing.
            let mut fetch = tokio::spawn({
                let (wallet, chain) = (Arc::clone(&wallet), Arc::clone(&chain));
                async move { wallet.fetch(&*chain).await }
            });
            let fetched = tokio::select! {
                () = stopping(&mut gate) => {
                    fetch.abort();
                    return Ok(());
                }
                _ = target.changed() => {
                    fetch.abort();
                    continue 'connect;
                }
                fetched = &mut fetch => fetched,
            };
            let fetched =
                fetched.unwrap_or_else(|_| Err(FedimixError::Internal(SYNC_PANICKED.into())));
            let synced = match fetched {
                Ok(fetched) => Arc::clone(&wallet).apply(fetched).await,
                Err(e) => Err(e),
            };
            match synced {
                Ok(summary) => {
                    failures = 0;
                    // After a sync that worked, as it reads the wallet's view.
                    // This client is the old route once the settings change.
                    let tended = tokio::select! {
                        biased;
                        () = stopping(&mut gate) => return Ok(()),
                        _ = target.changed() => continue 'connect,
                        tended = core.tend_signed(&wallet, &*chain) => tended,
                    };
                    // Labels are read by address, so a coin that came in
                    // during this sync is labelled already; it may land an exit.
                    if let Err(e) = core.land_exits(&wallet) {
                        eprintln!("fedimix: landing exits in Saving: {e}");
                    }
                    core.note_chain(true, None, summary.changed || tended);
                }
                Err(e) => {
                    failures += 1;
                    if e != FedimixError::Offline {
                        eprintln!("fedimix: syncing Saving: {e}");
                    }
                    core.note_chain(false, problem_of(&e), false);
                }
            }
            let delay = sync_delay(core.sync_interval(&wallet), failures);
            tokio::select! {
                () = stopping(&mut gate) => return Ok(()),
                _ = target.changed() => continue 'connect,
                () = tokio::time::sleep(delay) => {}
                () = core.saving_wake.notified() => {}
            }
        }
    }
}

/// Keeps `SavingState::device` current while a Send review is open, so the
/// review can say "Connect your device". A wait at the device polls by itself.
/// Nothing else polls: asking the USB stack is not free.
async fn poll_during_reviews(core: Arc<Core>) {
    let mut gate = core.gate.subscribe();
    loop {
        tokio::select! {
            () = stopping(&mut gate) => return,
            () = core.review_opened.notified() => {}
            () = tokio::time::sleep(DEVICE_POLL) => {}
        }
        if !core.review_open() {
            continue;
        }
        tokio::select! {
            () = stopping(&mut gate) => return,
            (_, problem) = core.poll_devices() => {
                if let Some(e) = problem {
                    eprintln!("fedimix: looking for the hardware wallet: {e}");
                }
            }
        }
    }
}

/// Ends the wait of the sync task, so the next sync starts now.
#[cfg(feature = "testing")]
pub fn sync_saving_now(backend: &super::RealBackend) {
    backend.core.saving_wake.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_back_off_to_five_minutes() {
        let delays = |interval| {
            (0..7)
                .map(|failures| sync_delay(interval, failures).as_secs())
                .collect::<Vec<_>>()
        };
        assert_eq!(delays(SYNC_INTERVAL), [60, 60, 120, 240, 300, 300, 300]);
        assert_eq!(delays(ACTIVE_SYNC_INTERVAL), [10, 10, 20, 40, 80, 160, 300]);
        assert_eq!(sync_delay(SYNC_INTERVAL, u32::MAX), MAX_SYNC_BACKOFF);
    }

    #[test]
    fn the_account_path_follows_the_network() {
        assert_eq!(account_path(Network::Bitcoin), "m/84'/0'/0'");
        assert_eq!(account_path(Network::Signet), "m/84'/1'/0'");
        assert_eq!(account_path(Network::Regtest), "m/84'/1'/0'");
    }
}
