//! Finding and opening the hardware wallets async-hwi supports: Ledger,
//! Coldcard, Jade and BitBox02. Specter has no policy-free signing here and
//! Trezor is not in async-hwi at all.
//!
//! Compile-only: no hardware was available when this was written, so none of
//! the `open` functions below has talked to a device. They follow the shapes in
//! async-hwi 0.0.35's own sources and the Phase 3 spike.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_hwi::bitbox::api::runtime::TokioRuntime;
use async_hwi::bitbox::api::{ConfigError, NoiseConfig, NoiseConfigData, Threading};
use async_hwi::bitbox::{BitBox02, PairingBitbox02, is_bitbox02};
use async_hwi::jade::{Jade, SerialTransport};
use async_hwi::ledger::{HidApi, Ledger};
use async_hwi::{Error as HwiError, HWI};
use async_trait::async_trait;

use super::seed_signer::SeedSigner;
use super::signer::{
    DevicePort, HwiSigner, Signer, kind_name, map_hwi_error, on_thread, still_waiting,
};
use super::wallet::DeviceAccount;
use crate::api::{DeviceInfo, DeviceKind, Network, SOFTWARE_DEVICE_ID, SOFTWARE_DEVICE_NAME};
use crate::error::{FedimixError, Result};

/// A device a scanner saw, before it is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Found {
    pub id: String,
    pub name: String,
    /// The transport path: an HID path, a serial port, a Coldcard serial number.
    pub path: String,
}

/// Looks for one kind of device.
#[async_trait]
pub(super) trait Scanner: Send + Sync {
    fn kind(&self) -> DeviceKind;
    async fn scan(&self) -> Result<Vec<Found>>;
    async fn open(
        &self,
        found: &Found,
        account: Option<&DeviceAccount>,
        busy: Arc<tokio::sync::Mutex<()>>,
    ) -> Result<Arc<dyn Signer>>;
}

fn kind_slug(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Ledger => "ledger",
        DeviceKind::Trezor => "trezor",
        DeviceKind::Coldcard => "coldcard",
        DeviceKind::Jade => "jade",
        DeviceKind::BitBox => "bitbox",
        DeviceKind::Software => "software",
    }
}

/// Stable while the device stays plugged in to the same port: the kind and the
/// fingerprint where the scan knows it, else the kind and the transport path.
/// Listing never opens a device (that would prompt BitBox pairing or a Jade
/// PIN), so every scanner here only knows the path.
pub(super) fn device_id(kind: DeviceKind, fingerprint: Option<&str>, path: &str) -> String {
    format!("{}:{}", kind_slug(kind), fingerprint.unwrap_or(path))
}

fn device_error(e: impl std::fmt::Debug) -> HwiError {
    HwiError::Device(format!("{e:?}"))
}

/// async-hwi's `bitcoin` network type is the one Fedimix uses.
fn btc_network(network: Network) -> bitcoin::Network {
    match network {
        Network::Bitcoin => bitcoin::Network::Bitcoin,
        Network::Signet => bitcoin::Network::Signet,
        Network::Regtest => bitcoin::Network::Regtest,
    }
}

/// The wallet policy Ledger signs `account` under: the default single-key
/// `wpkh(@0/**)` one, which needs no registration on the device.
pub(super) fn ledger_policy(account: &DeviceAccount) -> String {
    let path = account
        .path
        .trim_start_matches("m/")
        .trim_start_matches('m')
        .replace('h', "'");
    format!(
        "wpkh([{}/{}]{}/**)",
        account.fingerprint, path, account.xpub
    )
}

// ---- BitBox02 pairing key ----

/// BitBox02's noise pairing data, in `<dir>/noise.json` with mode 0600 inside a
/// 0700 directory. It holds the app's static private key for talking to the
/// device, which is why it is private; it can never sign a transaction.
pub(super) struct NoiseStore {
    dir: PathBuf,
}

impl NoiseStore {
    pub(super) fn new(dir: PathBuf) -> Self {
        NoiseStore { dir }
    }

    fn file(&self) -> PathBuf {
        self.dir.join("noise.json")
    }

    fn read(&self) -> std::result::Result<NoiseConfigData, String> {
        match std::fs::read_to_string(self.file()) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(NoiseConfigData::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn write(&self, data: &NoiseConfigData) -> std::result::Result<(), String> {
        let err = |e: std::io::Error| e.to_string();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(err)?;
        let text = serde_json::to_string(data).map_err(|e| e.to_string())?;
        // Written whole beside the file, then renamed over it, so a crash never
        // leaves half a key.
        let tmp = self.dir.join("noise.json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(err)?;
        // The mode above only applies to a new file.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(err)?;
        file.write_all(text.as_bytes()).map_err(err)?;
        file.sync_all().map_err(err)?;
        std::fs::rename(&tmp, self.file()).map_err(err)
    }
}

impl Threading for NoiseStore {}

impl NoiseConfig for NoiseStore {
    fn read_config(&self) -> std::result::Result<NoiseConfigData, ConfigError> {
        self.read().map_err(ConfigError)
    }

    fn store_config(&self, data: &NoiseConfigData) -> std::result::Result<(), ConfigError> {
        self.write(data).map_err(ConfigError)
    }
}

// ---- The port ----

/// The real [`DevicePort`]: every kind async-hwi supports.
pub struct HwiPort {
    scanners: Vec<Arc<dyn Scanner>>,
    /// One lock per device id, held by a signer while a device call runs.
    busy: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl HwiPort {
    /// `data_dir` holds BitBox02's pairing key; `network` tells Jade and
    /// BitBox02 which coin to use.
    pub fn new(data_dir: &Path, network: Network) -> Self {
        let scanners: Vec<Arc<dyn Scanner>> = vec![
            Arc::new(LedgerScanner),
            Arc::new(ColdcardScanner),
            Arc::new(JadeScanner {
                network: btc_network(network),
            }),
            Arc::new(BitBoxScanner {
                network: btc_network(network),
                noise_dir: data_dir.join("bitbox"),
            }),
        ];
        HwiPort::with_scanners_inner(scanners)
    }

    fn with_scanners_inner(scanners: Vec<Arc<dyn Scanner>>) -> Self {
        HwiPort {
            scanners,
            busy: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(super) fn with_scanners(scanners: Vec<Arc<dyn Scanner>>) -> Self {
        HwiPort::with_scanners_inner(scanners)
    }

    fn busy_for(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.busy
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    /// Every scanner's answer, in order. One kind failing leaves the rest.
    async fn scan_all(&self) -> Vec<(Arc<dyn Scanner>, Result<Vec<Found>>)> {
        let mut out = Vec::new();
        for scanner in &self.scanners {
            let found = scanner.scan().await;
            out.push((scanner.clone(), found));
        }
        out
    }
}

#[async_trait]
impl DevicePort for HwiPort {
    async fn list(&self) -> Result<Vec<DeviceInfo>> {
        let mut devices = Vec::new();
        let mut first_error = None;
        let mut seen = HashSet::new();
        for (scanner, found) in self.scan_all().await {
            match found {
                Ok(found) => {
                    for f in found {
                        if seen.insert(f.id.clone()) {
                            devices.push(DeviceInfo {
                                id: f.id,
                                name: f.name,
                                kind: scanner.kind(),
                                connected: true,
                            });
                        }
                    }
                }
                Err(e) => first_error = first_error.or(Some(e)),
            }
        }
        // An error is only worth showing when it left us with nothing: the
        // person's device may be the very one that could not be looked for.
        match (devices.is_empty(), first_error) {
            (true, Some(e)) => Err(e),
            _ => Ok(devices),
        }
    }

    async fn connect(&self, id: &str, account: Option<&DeviceAccount>) -> Result<Arc<dyn Signer>> {
        for (scanner, found) in self.scan_all().await {
            let Some(f) = found.ok().and_then(|v| v.into_iter().find(|f| f.id == id)) else {
                continue;
            };
            // A cancelled call may still be running on this device; a second
            // handle on it could interleave with that command.
            let busy = self.busy_for(id);
            drop(busy.try_lock().map_err(|_| still_waiting())?);
            let signer = scanner.open(&f, account, busy).await?;
            if let Some(account) = account {
                let name = kind_name(scanner.kind());
                let wrong = || {
                    FedimixError::Internal(format!(
                        "The {name} you connected is not the device Saving was set up with. Connect the right one."
                    ))
                };
                if scanner.kind() != account.kind {
                    return Err(wrong());
                }
                let fingerprint = signer.fingerprint().await?;
                if !fingerprint.eq_ignore_ascii_case(&account.fingerprint) {
                    return Err(wrong());
                }
            }
            return Ok(signer);
        }
        Err(FedimixError::DeviceNotConnected("hardware wallet".into()))
    }
}

// ---- The software wallet ----

const OTHER_WORDS: &str = "The recovery words in this folder are not the ones Saving was set up with, so they cannot sign for it. Restore the seed file from your backup.";

/// The devices `RealBackend` offers: whatever its hardware [`DevicePort`]
/// finds, then always the software wallet, as it needs nothing plugged in.
pub struct WithSoftware {
    hardware: Arc<dyn DevicePort>,
    software: Arc<SeedSigner>,
}

/// How the software wallet is listed.
pub fn software_entry() -> DeviceInfo {
    DeviceInfo {
        id: SOFTWARE_DEVICE_ID.to_string(),
        name: SOFTWARE_DEVICE_NAME.to_string(),
        kind: DeviceKind::Software,
        connected: true,
    }
}

impl WithSoftware {
    pub fn new(hardware: Arc<dyn DevicePort>, software: Arc<SeedSigner>) -> Self {
        WithSoftware { hardware, software }
    }

    /// The hardware wallets only, with the hardware port's own error when its
    /// scan failed and found nothing.
    pub async fn hardware(&self) -> Result<Vec<DeviceInfo>> {
        let mut devices = self.hardware.list().await?;
        devices.retain(|device| device.id != SOFTWARE_DEVICE_ID);
        Ok(devices)
    }

    /// The hardware wallets, then the software wallet. A failed hardware scan
    /// does not hide the software wallet: its error comes back beside it.
    pub async fn list(&self) -> (Vec<DeviceInfo>, Option<FedimixError>) {
        let (mut devices, problem) = match self.hardware().await {
            Ok(devices) => (devices, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        devices.push(software_entry());
        (devices, problem)
    }

    /// Opens `id`. The software wallet is checked as a device is: with an
    /// account, it must be a software one with this fingerprint.
    pub async fn connect(
        &self,
        id: &str,
        account: Option<&DeviceAccount>,
    ) -> Result<Arc<dyn Signer>> {
        if id != SOFTWARE_DEVICE_ID {
            return self.hardware.connect(id, account).await;
        }
        if let Some(account) = account {
            if account.kind != DeviceKind::Software {
                let name = kind_name(account.kind);
                return Err(FedimixError::Internal(format!(
                    "Saving was set up with your {name}, not with this computer. Connect your {name}."
                )));
            }
            let fingerprint = self.software.fingerprint().await?;
            if !fingerprint.eq_ignore_ascii_case(&account.fingerprint) {
                return Err(FedimixError::Internal(OTHER_WORDS.into()));
            }
        }
        Ok(Arc::clone(&self.software) as Arc<dyn Signer>)
    }
}

// ---- Ledger ----

struct LedgerScanner;

#[async_trait]
impl Scanner for LedgerScanner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Ledger
    }

    async fn scan(&self) -> Result<Vec<Found>> {
        on_thread(|_| {
            let api = HidApi::new().map_err(device_error)?;
            Ok(Ledger::enumerate(&api)
                .map(|d| {
                    let path = d.path().to_string_lossy().into_owned();
                    Found {
                        id: device_id(DeviceKind::Ledger, None, &path),
                        name: "Ledger".into(),
                        path,
                    }
                })
                .collect())
        })
        .await?
        .map_err(|e: HwiError| map_hwi_error(DeviceKind::Ledger, &e))
    }

    // Untested without a device. Ledger signs only under a wallet policy:
    // `Ledger::with_wallet` with the default `wpkh(@0/**)` policy and
    // `hmac = None` for the account the DeviceAccount holds, as the Ledger app
    // treats that policy as standard. The name of a default policy is empty.
    async fn open(
        &self,
        found: &Found,
        account: Option<&DeviceAccount>,
        busy: Arc<tokio::sync::Mutex<()>>,
    ) -> Result<Arc<dyn Signer>> {
        let path = found.path.clone();
        let account = account.filter(|a| a.kind == DeviceKind::Ledger).cloned();
        let ledger = on_thread(move |handle| -> std::result::Result<_, HwiError> {
            let api = HidApi::new().map_err(device_error)?;
            let info = Ledger::enumerate(&api)
                .find(|d| d.path().to_string_lossy() == path.as_str())
                .ok_or(HwiError::DeviceNotFound)?;
            let ledger = Ledger::connect(&api, info)?;
            match account {
                Some(account) => {
                    // A different Ledger keeps no policy: it cannot sign for
                    // this account, and Task 6 would see no signatures.
                    let fingerprint = handle.block_on(ledger.get_master_fingerprint())?;
                    if fingerprint.to_string() == account.fingerprint {
                        ledger.with_wallet("", &ledger_policy(&account), None)
                    } else {
                        Ok(ledger)
                    }
                }
                None => Ok(ledger),
            }
        })
        .await?
        .map_err(|e| map_hwi_error(DeviceKind::Ledger, &e))?;
        Ok(Arc::new(HwiSigner::new(ledger, DeviceKind::Ledger, busy)))
    }
}

// ---- Coldcard ----

struct ColdcardScanner;

#[async_trait]
impl Scanner for ColdcardScanner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Coldcard
    }

    async fn scan(&self) -> Result<Vec<Found>> {
        on_thread(|_| {
            let mut api = async_hwi::coldcard::api::Api::new().map_err(device_error)?;
            Ok(api
                .detect()
                .map_err(device_error)?
                .iter()
                .map(|serial| {
                    let path = serial.as_ref().to_string();
                    Found {
                        id: device_id(DeviceKind::Coldcard, None, &path),
                        name: "Coldcard".into(),
                        path,
                    }
                })
                .collect())
        })
        .await?
        .map_err(|e: HwiError| map_hwi_error(DeviceKind::Coldcard, &e))
    }

    async fn open(
        &self,
        found: &Found,
        _account: Option<&DeviceAccount>,
        busy: Arc<tokio::sync::Mutex<()>>,
    ) -> Result<Arc<dyn Signer>> {
        let serial = found.path.clone();
        let coldcard = on_thread(move |_| -> std::result::Result<_, HwiError> {
            let api = async_hwi::coldcard::api::Api::new().map_err(device_error)?;
            let (coldcard, _) = api.open(serial, None).map_err(device_error)?;
            Ok(async_hwi::coldcard::Coldcard::from(coldcard))
        })
        .await?
        .map_err(|e| map_hwi_error(DeviceKind::Coldcard, &e))?;
        Ok(Arc::new(HwiSigner::new(
            coldcard,
            DeviceKind::Coldcard,
            busy,
        )))
    }
}

// ---- Jade ----

struct JadeScanner {
    network: bitcoin::Network,
}

#[async_trait]
impl Scanner for JadeScanner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Jade
    }

    // Lists serial ports with a Jade's USB ids without opening them: a poll
    // must not grab the port a signing Jade is using. The cost is that another
    // device with the same USB-serial chip also shows up, and fails on connect.
    async fn scan(&self) -> Result<Vec<Found>> {
        on_thread(|_| {
            Ok(SerialTransport::enumerate_potential_ports()
                .map_err(HwiError::from)?
                .into_iter()
                .map(|path| Found {
                    id: device_id(DeviceKind::Jade, None, &path),
                    name: "Jade".into(),
                    path,
                })
                .collect())
        })
        .await?
        .map_err(|e: HwiError| map_hwi_error(DeviceKind::Jade, &e))
    }

    // `auth` may need the PIN on the device, and then talks to Blockstream's
    // PIN server over plain HTTPS, outside any proxy set in Settings.
    async fn open(
        &self,
        found: &Found,
        _account: Option<&DeviceAccount>,
        busy: Arc<tokio::sync::Mutex<()>>,
    ) -> Result<Arc<dyn Signer>> {
        let (path, network) = (found.path.clone(), self.network);
        let jade = on_thread(move |handle| -> std::result::Result<_, HwiError> {
            let transport = SerialTransport::new(path).map_err(|_| HwiError::DeviceNotFound)?;
            let jade = Jade::new(transport).with_network(network);
            handle.block_on(async {
                jade.ping().await.map_err(HwiError::from)?;
                jade.auth().await.map_err(HwiError::from)
            })?;
            Ok(jade)
        })
        .await?
        .map_err(|e| map_hwi_error(DeviceKind::Jade, &e))?;
        Ok(Arc::new(HwiSigner::new(jade, DeviceKind::Jade, busy)))
    }
}

// ---- BitBox02 ----

struct BitBoxScanner {
    network: bitcoin::Network,
    noise_dir: PathBuf,
}

#[async_trait]
impl Scanner for BitBoxScanner {
    fn kind(&self) -> DeviceKind {
        DeviceKind::BitBox
    }

    async fn scan(&self) -> Result<Vec<Found>> {
        on_thread(|_| {
            let api = HidApi::new().map_err(device_error)?;
            Ok(api
                .device_list()
                .filter(|d| is_bitbox02(d))
                .map(|d| {
                    let path = d.path().to_string_lossy().into_owned();
                    Found {
                        id: device_id(DeviceKind::BitBox, None, &path),
                        name: "BitBox02".into(),
                        path,
                    }
                })
                .collect())
        })
        .await?
        .map_err(|e: HwiError| map_hwi_error(DeviceKind::BitBox, &e))
    }

    // Pairing: the first connection shows a code on the device and on a
    // computer; `wait_confirm` returns once the user confirms on the device,
    // which can take as long as the user takes. Later connections find the
    // device's key in the noise store and need no confirmation.
    async fn open(
        &self,
        found: &Found,
        _account: Option<&DeviceAccount>,
        busy: Arc<tokio::sync::Mutex<()>>,
    ) -> Result<Arc<dyn Signer>> {
        let (path, network, dir) = (found.path.clone(), self.network, self.noise_dir.clone());
        let bitbox = on_thread(move |handle| -> std::result::Result<_, HwiError> {
            let api = HidApi::new().map_err(device_error)?;
            let info = api
                .device_list()
                .find(|d| is_bitbox02(d) && d.path().to_string_lossy() == path.as_str())
                .ok_or(HwiError::DeviceNotFound)?;
            let device = info
                .open_device(&api)
                .map_err(|_| HwiError::DeviceNotFound)?;
            handle.block_on(async {
                let pairing = PairingBitbox02::<TokioRuntime>::connect(
                    device,
                    Some(Box::new(NoiseStore::new(dir))),
                )
                .await?;
                let paired = pairing.wait_confirm().await?;
                Ok(BitBox02::from(paired).with_network(network))
            })
        })
        .await?
        .map_err(|e| map_hwi_error(DeviceKind::BitBox, &e))?;
        Ok(Arc::new(HwiSigner::new(bitbox, DeviceKind::BitBox, busy)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn account(kind: DeviceKind) -> DeviceAccount {
        DeviceAccount {
            kind,
            name: "Ledger".into(),
            fingerprint: "aabbccdd".into(),
            xpub: "tpubDExA3EC3iAsPxPhFn4j6gMiVup6V2eH3qKyk69RcTc9TTNRfFYVPad8bJD5FCHVQxyBT4izKsvr7Btd2R4xmQ1hZkvsqGBaeE82J71uTK4N".into(),
            path: "m/84'/1'/0'".into(),
        }
    }

    #[test]
    fn ids_use_the_fingerprint_when_known_else_the_path() {
        assert_eq!(
            device_id(DeviceKind::Ledger, Some("aabbccdd"), "/dev/hidraw3"),
            "ledger:aabbccdd"
        );
        assert_eq!(
            device_id(DeviceKind::Jade, None, "/dev/ttyUSB0"),
            "jade:/dev/ttyUSB0"
        );
        assert_ne!(
            device_id(DeviceKind::Ledger, None, "/dev/hidraw3"),
            device_id(DeviceKind::BitBox, None, "/dev/hidraw3")
        );
    }

    #[test]
    fn the_ledger_policy_is_the_default_single_key_one() {
        let policy = ledger_policy(&account(DeviceKind::Ledger));
        assert!(
            policy.starts_with("wpkh([aabbccdd/84'/1'/0']tpub"),
            "{policy}"
        );
        assert!(policy.ends_with("/**)"), "{policy}");
        let (template, keys) =
            async_hwi::utils::extract_keys_and_template::<String>(&policy).unwrap();
        assert_eq!(template, "wpkh(@0/**)");
        assert_eq!(keys.len(), 1);
        // The h spelling is normalised.
        let mut a = account(DeviceKind::Ledger);
        a.path = "m/84h/1h/0h".into();
        assert_eq!(ledger_policy(&a), policy);
    }

    #[test]
    fn the_noise_key_is_stored_0600_in_a_private_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bitbox");
        let store = NoiseStore::new(dir.clone());
        assert!(store.read_config().unwrap().app_static_privkey.is_none());
        let data = NoiseConfigData {
            app_static_privkey: Some([9; 32]),
            device_static_pubkeys: vec![vec![1, 2, 3]],
        };
        store.store_config(&data).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("noise.json")), 0o600);
        let back = store.read_config().unwrap();
        assert_eq!(back.app_static_privkey, Some([9; 32]));
        assert_eq!(back.device_static_pubkeys, vec![vec![1, 2, 3]]);
        // A file that was loosened is tightened on the next write.
        std::fs::set_permissions(
            dir.join("noise.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::set_permissions(
            dir.join("noise.json.tmp"),
            std::fs::Permissions::from_mode(0o644),
        )
        .ok();
        store.store_config(&data).unwrap();
        assert_eq!(mode(&dir.join("noise.json")), 0o600);
        // A damaged file is an error, not a silent re-pairing.
        std::fs::write(dir.join("noise.json"), "{").unwrap();
        assert!(store.read_config().is_err());
    }

    struct FakeScanner {
        kind: DeviceKind,
        found: std::result::Result<Vec<&'static str>, FedimixError>,
        opened: Mutex<Vec<String>>,
    }

    impl FakeScanner {
        fn ok(kind: DeviceKind, paths: &[&'static str]) -> Arc<Self> {
            Arc::new(FakeScanner {
                kind,
                found: Ok(paths.to_vec()),
                opened: Mutex::new(vec![]),
            })
        }
        fn failing(kind: DeviceKind) -> Arc<Self> {
            Arc::new(FakeScanner {
                kind,
                found: Err(FedimixError::Internal("Could not look.".into())),
                opened: Mutex::new(vec![]),
            })
        }
    }

    #[async_trait]
    impl Scanner for FakeScanner {
        fn kind(&self) -> DeviceKind {
            self.kind
        }
        async fn scan(&self) -> Result<Vec<Found>> {
            self.found.clone().map(|paths| {
                paths
                    .into_iter()
                    .map(|p| Found {
                        id: device_id(self.kind, None, p),
                        name: super::super::signer::kind_name(self.kind).into(),
                        path: p.into(),
                    })
                    .collect()
            })
        }
        async fn open(
            &self,
            found: &Found,
            _account: Option<&DeviceAccount>,
            _busy: Arc<tokio::sync::Mutex<()>>,
        ) -> Result<Arc<dyn Signer>> {
            self.opened.lock().unwrap().push(found.path.clone());
            Ok(Arc::new(crate::saving::SoftSigner::from_seed(
                &[1; 32],
                bitcoin::Network::Regtest,
            )))
        }
    }

    #[tokio::test]
    async fn one_kind_failing_never_hides_the_others() {
        let port = HwiPort::with_scanners(vec![
            FakeScanner::failing(DeviceKind::Ledger),
            FakeScanner::ok(DeviceKind::Coldcard, &["SN1"]),
            FakeScanner::ok(DeviceKind::Jade, &["/dev/ttyUSB0", "/dev/ttyUSB1"]),
            FakeScanner::failing(DeviceKind::BitBox),
        ]);
        let list = port.list().await.unwrap();
        let ids: Vec<_> = list.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            ["coldcard:SN1", "jade:/dev/ttyUSB0", "jade:/dev/ttyUSB1"]
        );
        assert!(list.iter().all(|d| d.connected));
        assert_eq!(list[0].kind, DeviceKind::Coldcard);
        assert_eq!(list[0].name, "Coldcard");
    }

    #[tokio::test]
    async fn listing_reports_an_error_only_when_it_found_nothing() {
        let port = HwiPort::with_scanners(vec![
            FakeScanner::failing(DeviceKind::Ledger),
            FakeScanner::ok(DeviceKind::Jade, &[]),
        ]);
        assert_eq!(
            port.list().await,
            Err(FedimixError::Internal("Could not look.".into()))
        );
        let port = HwiPort::with_scanners(vec![
            FakeScanner::ok(DeviceKind::Ledger, &[]),
            FakeScanner::ok(DeviceKind::Jade, &[]),
        ]);
        assert_eq!(port.list().await, Ok(vec![]));
    }

    #[tokio::test]
    async fn connect_opens_the_scanner_that_listed_the_id() {
        let ledger = FakeScanner::ok(DeviceKind::Ledger, &["/dev/hidraw1"]);
        let jade = FakeScanner::ok(DeviceKind::Jade, &["/dev/ttyUSB0"]);
        let port = HwiPort::with_scanners(vec![
            FakeScanner::failing(DeviceKind::Coldcard),
            ledger.clone(),
            jade.clone(),
        ]);
        port.connect("jade:/dev/ttyUSB0", None).await.unwrap();
        assert_eq!(*jade.opened.lock().unwrap(), vec!["/dev/ttyUSB0"]);
        assert!(ledger.opened.lock().unwrap().is_empty());
        assert!(matches!(
            port.connect("ledger:/dev/gone", None).await,
            Err(FedimixError::DeviceNotConnected(_))
        ));
    }

    fn soft_fingerprint() -> String {
        let soft = crate::saving::SoftSigner::from_seed(&[1; 32], bitcoin::Network::Regtest);
        futures::executor::block_on(soft.fingerprint()).unwrap()
    }

    #[tokio::test]
    async fn connect_refuses_a_device_that_is_not_the_one_saving_was_set_up_with() {
        let jade = FakeScanner::ok(DeviceKind::Jade, &["/dev/ttyUSB0"]);
        let port = HwiPort::with_scanners(vec![jade]);
        let mut account = account(DeviceKind::Jade);
        account.fingerprint = "00000000".into();
        let refused = port.connect("jade:/dev/ttyUSB0", Some(&account)).await;
        match refused {
            Err(FedimixError::Internal(s)) => {
                assert!(s.contains("Jade"), "names the device: {s}");
                assert!(s.ends_with('.'), "{s}");
            }
            other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
        }
        // The right fingerprint (in either case) and no account both connect.
        account.fingerprint = soft_fingerprint().to_uppercase();
        port.connect("jade:/dev/ttyUSB0", Some(&account))
            .await
            .unwrap();
        port.connect("jade:/dev/ttyUSB0", None).await.unwrap();
        // The same fingerprint under another kind is another device.
        account.kind = DeviceKind::Ledger;
        assert!(
            port.connect("jade:/dev/ttyUSB0", Some(&account))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn connect_to_a_device_with_an_abandoned_call_is_told_at_once() {
        let jade = FakeScanner::ok(DeviceKind::Jade, &["/dev/ttyUSB0"]);
        let port = HwiPort::with_scanners(vec![jade.clone()]);
        // A call that was cancelled but still runs holds the device's lock.
        let held = port.busy_for("jade:/dev/ttyUSB0").try_lock_owned().unwrap();
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            port.connect("jade:/dev/ttyUSB0", None),
        )
        .await
        .expect("connect must not wait");
        assert_eq!(
            blocked.map(|_| ()),
            Err(FedimixError::Internal(
                "Your device is still waiting for an answer. Reject it on the device, then try again.".into()
            ))
        );
        assert!(
            jade.opened.lock().unwrap().is_empty(),
            "no second handle was opened"
        );
        drop(held);
        port.connect("jade:/dev/ttyUSB0", None).await.unwrap();
    }

    const WORDS: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn with_software(scanners: Vec<Arc<dyn Scanner>>) -> WithSoftware {
        let seed = Arc::new(crate::seed::Seed::from_phrase(WORDS));
        WithSoftware::new(
            Arc::new(HwiPort::with_scanners(scanners)),
            Arc::new(SeedSigner::new(seed, Network::Regtest)),
        )
    }

    fn the_software_entry() -> DeviceInfo {
        DeviceInfo {
            id: "software".into(),
            name: "This computer".into(),
            kind: DeviceKind::Software,
            connected: true,
        }
    }

    #[tokio::test]
    async fn software_is_listed_after_devices() {
        let port = with_software(vec![
            FakeScanner::ok(DeviceKind::Ledger, &["/dev/hidraw1"]),
            FakeScanner::ok(DeviceKind::Jade, &["/dev/ttyUSB0"]),
        ]);
        let (list, problem) = port.list().await;
        assert_eq!(problem, None);
        let ids: Vec<_> = list.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            ["ledger:/dev/hidraw1", "jade:/dev/ttyUSB0", "software"]
        );
        assert_eq!(list[2], the_software_entry());
        assert_eq!(
            with_software(vec![]).list().await,
            (vec![the_software_entry()], None)
        );
    }

    #[tokio::test]
    async fn software_is_listed_when_no_device_answers() {
        let port = with_software(vec![
            FakeScanner::failing(DeviceKind::Ledger),
            FakeScanner::failing(DeviceKind::BitBox),
        ]);
        // The scan's own sentence comes back beside the software wallet.
        assert_eq!(
            port.list().await,
            (
                vec![the_software_entry()],
                Some(FedimixError::Internal("Could not look.".into()))
            )
        );
        assert_eq!(
            port.hardware().await,
            Err(FedimixError::Internal("Could not look.".into()))
        );
    }

    #[tokio::test]
    async fn software_connects_only_as_the_account_it_set_up() {
        let jade = FakeScanner::ok(DeviceKind::Jade, &["/dev/ttyUSB0"]);
        let port = with_software(vec![jade.clone()]);
        let signer = port.connect("software", None).await.unwrap();
        assert_eq!(signer.kind(), DeviceKind::Software);
        assert_eq!(signer.fingerprint().await.unwrap(), "73c5da0a");
        let mut account = account(DeviceKind::Software);
        account.fingerprint = "73C5DA0A".into();
        port.connect("software", Some(&account)).await.unwrap();
        // Other words in the seed file: another wallet.
        account.fingerprint = "aabbccdd".into();
        match port.connect("software", Some(&account)).await {
            Err(FedimixError::Internal(s)) => {
                assert!(s.contains("recovery words"), "{s}");
                assert!(!s.contains("abandon"), "{s}");
            }
            other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
        }
        // A hardware account never connects to the software wallet.
        let mut account = account.clone();
        account.kind = DeviceKind::Ledger;
        account.fingerprint = "73c5da0a".into();
        assert!(port.connect("software", Some(&account)).await.is_err());
        // Hardware ids go to the hardware port.
        port.connect("jade:/dev/ttyUSB0", None).await.unwrap();
        assert_eq!(*jade.opened.lock().unwrap(), vec!["/dev/ttyUSB0"]);
    }

    #[test]
    fn the_real_port_lists_every_supported_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let port = HwiPort::new(tmp.path(), Network::Regtest);
        let kinds: Vec<_> = port.scanners.iter().map(|s| s.kind()).collect();
        assert_eq!(
            kinds,
            [
                DeviceKind::Ledger,
                DeviceKind::Coldcard,
                DeviceKind::Jade,
                DeviceKind::BitBox
            ]
        );
        assert!(
            !tmp.path().join("bitbox").exists(),
            "nothing is created before pairing"
        );
    }
}
