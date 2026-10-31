//! The watch-only Saving wallet: a BDK wallet built from the hardware wallet's
//! account xpub alone. It can list coins, show history and hand out addresses;
//! it cannot sign, and no private key is ever given to it or stored beside it.
//!
//! BDK keeps its tables in `fedimix.db` through its own connection, beside the
//! tables of [`Store`]. Both connections use WAL and a busy timeout, and every
//! BDK write takes the write lock up front (see [`Db`]), so the two wait for
//! each other instead of failing.
//!
//! The wallet sits behind a `Mutex` that is never held across an await: a sync
//! takes its request out under the lock, waits for the chain without it, and
//! applies the answer under the lock again.

use std::path::Path;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use bdk_wallet::Wallet;
use bdk_wallet::bitcoin::bip32::{DerivationPath, Fingerprint, Xpub};
use bdk_wallet::bitcoin::{self, Address, NetworkKind};
use bdk_wallet::chain::ChainPosition;
use bdk_wallet::chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_wallet::rusqlite::{Connection, Transaction, TransactionBehavior};
use bdk_wallet::{
    ChangeSet, CreateWithPersistError, KeychainKind, LoadError, LoadMismatch, LoadWithPersistError,
    PersistedWallet, Update, WalletPersister,
};

use crate::api::{Amount, Coin, DeviceKind, HistoryItem, HistoryKind, HistoryStatus, Network};
use crate::error::{FedimixError, Result};
use crate::saving::{ChainClient, Labels, Tag};
use crate::store::{Store, db_err};

/// How far a first scan looks past the last used address. Fedimix reveals an
/// address per receive, so a restored wallet may have long runs of unused ones.
const FULL_SCAN_STOP_GAP: usize = 100;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The hardware wallet's account: public data only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAccount {
    pub kind: DeviceKind,
    pub name: String,
    /// Master fingerprint, 8 hex digits.
    pub fingerprint: String,
    /// The account-level extended public key (`xpub...` or `tpub...`).
    pub xpub: String,
    /// The account path, such as `m/84'/0'/0'`.
    pub path: String,
}

/// What a sync did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncSummary {
    /// It was the first scan of this wallet (`stop_gap` 100), not a routine sync.
    pub full_scan: bool,
    /// The wallet learned something new, and it is persisted.
    pub changed: bool,
}

/// BDK's rusqlite persister, with the write lock taken when a transaction
/// begins. BDK's own impl for `Connection` begins deferred, so a write racing
/// the store's could fail with `SQLITE_BUSY` at once instead of waiting.
struct Db(Connection);

impl Db {
    fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).map_err(db_err)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(db_err)?;
        // The journal mode is the file's, set by the store; `synchronous` is
        // per connection and must match the store's.
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(db_err)?;
        Ok(Db(conn))
    }
}

impl WalletPersister for Db {
    type Error = bdk_wallet::rusqlite::Error;

    fn initialize(persister: &mut Self) -> std::result::Result<ChangeSet, Self::Error> {
        let mut tx = Transaction::new(&mut persister.0, TransactionBehavior::Immediate)?;
        let changeset = <Transaction<'_> as WalletPersister>::initialize(&mut tx)?;
        tx.commit()?;
        Ok(changeset)
    }

    fn persist(
        persister: &mut Self,
        changeset: &ChangeSet,
    ) -> std::result::Result<(), Self::Error> {
        let mut tx = Transaction::new(&mut persister.0, TransactionBehavior::Immediate)?;
        <Transaction<'_> as WalletPersister>::persist(&mut tx, changeset)?;
        tx.commit()
    }
}

struct Inner {
    wallet: PersistedWallet<Db>,
    db: Db,
}

impl Inner {
    /// Writes what the wallet has staged. Called after every change.
    fn persist(&mut self) -> Result<bool> {
        self.wallet.persist(&mut self.db).map_err(db_err)
    }
}

pub struct SavingWallet {
    network: Network,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for SavingWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavingWallet")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

fn btc_network(network: Network) -> bitcoin::Network {
    match network {
        Network::Bitcoin => bitcoin::Network::Bitcoin,
        Network::Signet => bitcoin::Network::Signet,
        Network::Regtest => bitcoin::Network::Regtest,
    }
}

fn bad_account(what: &str) -> FedimixError {
    FedimixError::NotSetUp(format!(
        "The hardware wallet gave {what} that Fedimix cannot read. Reconnect it and set up Saving again."
    ))
}

/// The external and internal descriptors of `account`, checked against
/// `network`. Built from parsed parts, so odd text in the account cannot
/// change the descriptor's shape.
fn descriptors(account: &DeviceAccount, network: Network) -> Result<(String, String)> {
    let fingerprint =
        Fingerprint::from_str(&account.fingerprint).map_err(|_| bad_account("a fingerprint"))?;
    let xpub = Xpub::from_str(&account.xpub).map_err(|_| bad_account("a public key"))?;
    let path_text = account.path.strip_prefix("m/").unwrap_or(&account.path);
    let path = DerivationPath::from_str(path_text).map_err(|_| bad_account("a key path"))?;
    let expected = NetworkKind::from(btc_network(network));
    if xpub.network != expected {
        let (has, wants) = if expected == NetworkKind::Main {
            ("test networks", "the main network")
        } else {
            ("the main network", "a test network")
        };
        return Err(FedimixError::NotSetUp(format!(
            "The hardware wallet account is for {has}, but Fedimix is running on {wants}."
        )));
    }
    let origin = format!("[{fingerprint}/{path}]{xpub}");
    Ok((format!("wpkh({origin}/0/*)"), format!("wpkh({origin}/1/*)")))
}

impl SavingWallet {
    /// Stores the account and creates the wallet. Refused when Saving is
    /// already set up. Nothing is written for an account that does not fit
    /// `network`.
    pub fn create(
        store_path: &Path,
        account: DeviceAccount,
        network: Network,
    ) -> Result<SavingWallet> {
        descriptors(&account, network)?;
        let store = Store::open(store_path)?;
        if !store.set_device_account(&account)? {
            return Err(FedimixError::NotSetUp(
                "Saving is already set up with a hardware wallet.".into(),
            ));
        }
        Self::attach(store_path, &account, network)
    }

    /// The wallet of a Saving that was set up, or `None`.
    pub fn open(store_path: &Path, network: Network) -> Result<Option<SavingWallet>> {
        let store = Store::open(store_path)?;
        let Some(account) = store.device_account()? else {
            return Ok(None);
        };
        Self::attach(store_path, &account, network).map(Some)
    }

    /// Loads the BDK wallet from the database, or creates it if the account
    /// was stored but a crash came before the wallet's first write.
    fn attach(
        store_path: &Path,
        account: &DeviceAccount,
        network: Network,
    ) -> Result<SavingWallet> {
        let (external, internal) = descriptors(account, network)?;
        let btc = btc_network(network);
        let mut db = Db::open(store_path)?;
        let loaded = Wallet::load()
            .descriptor(KeychainKind::External, Some(external.clone()))
            .descriptor(KeychainKind::Internal, Some(internal.clone()))
            .check_network(btc)
            .load_wallet(&mut db);
        let wallet = match loaded {
            Ok(Some(wallet)) => wallet,
            Ok(None) => Wallet::create(external, internal)
                .network(btc)
                .create_wallet(&mut db)
                .map_err(|e| match e {
                    CreateWithPersistError::Persist(e) => db_err(e),
                    other => FedimixError::Internal(format!(
                        "Fedimix could not create the Saving wallet: {other}."
                    )),
                })?,
            Err(LoadWithPersistError::InvalidChangeSet(LoadError::Mismatch(
                LoadMismatch::Network { loaded, .. },
            ))) => {
                return Err(FedimixError::WrongNetwork {
                    folder: loaded.to_string(),
                    wanted: network.name().to_string(),
                });
            }
            Err(LoadWithPersistError::Persist(e)) => return Err(db_err(e)),
            Err(LoadWithPersistError::InvalidChangeSet(e)) => {
                return Err(FedimixError::Internal(format!(
                    "The Saving wallet in this data folder does not match the hardware wallet: {e}."
                )));
            }
        };
        Ok(SavingWallet {
            network,
            inner: Mutex::new(Inner { wallet, db }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Asks the chain what changed and applies the answer. The first sync of a
    /// wallet is a full scan (`stop_gap` 100); later ones look only at the
    /// scripts already revealed. The lock is not held while waiting.
    pub async fn sync(&self, chain: &dyn ChainClient) -> Result<SyncSummary> {
        enum Request {
            Full(FullScanRequest<KeychainKind>),
            Routine(SyncRequest<(KeychainKind, u32)>),
        }
        // A wallet that never heard from a chain server knows only genesis.
        let request = {
            let inner = self.lock();
            if inner.wallet.latest_checkpoint().height() == 0 {
                Request::Full(inner.wallet.start_full_scan().build())
            } else {
                Request::Routine(inner.wallet.start_sync_with_revealed_spks().build())
            }
        };
        match request {
            Request::Full(request) => {
                let response = chain.full_scan(request, FULL_SCAN_STOP_GAP).await?;
                let changed = self.apply_update(response.into())?;
                Ok(SyncSummary {
                    full_scan: true,
                    changed,
                })
            }
            Request::Routine(request) => {
                let response = chain.sync(request).await?;
                let changed = self.apply_update(response.into())?;
                Ok(SyncSummary {
                    full_scan: false,
                    changed,
                })
            }
        }
    }

    /// Applies `update` and persists. `true` when anything changed.
    pub(crate) fn apply_update(&self, update: Update) -> Result<bool> {
        let mut inner = self.lock();
        inner.wallet.apply_update(update).map_err(|_| {
            FedimixError::Internal(
                "The chain server's answer does not continue the chain Fedimix last saw. Try again in a moment."
                    .into(),
            )
        })?;
        inner.persist()
    }

    /// What is yours and settled, plus change from your own unconfirmed sends.
    pub fn balance(&self) -> Amount {
        let balance = self.lock().wallet.balance();
        sat(balance.confirmed + balance.trusted_pending)
    }

    /// Incoming and not yet confirmed, from someone else.
    pub fn in_flight(&self) -> Amount {
        sat(self.lock().wallet.balance().untrusted_pending)
    }

    /// Every coin the wallet owns, frozen and unconfirmed included.
    pub fn coins(&self, labels: &Labels) -> Vec<Coin> {
        let inner = self.lock();
        let mut coins: Vec<Coin> = inner
            .wallet
            .list_unspent()
            .map(|utxo| {
                let outpoint = utxo.outpoint.to_string();
                let label = labels.get(&outpoint);
                Coin {
                    amount: sat(utxo.txout.value),
                    tag: labels.tag(&outpoint).to_public(),
                    received_at: position_time(&utxo.chain_position),
                    address: Address::from_script(
                        &utxo.txout.script_pubkey,
                        btc_network(self.network),
                    )
                    .map(|a| a.to_string())
                    .unwrap_or_default(),
                    source_federation: label.and_then(|l| l.source_federation.clone()),
                    frozen: label.is_some_and(|l| l.frozen),
                    outpoint,
                }
            })
            .collect();
        coins.sort_by(|a, b| {
            b.received_at
                .cmp(&a.received_at)
                .then_with(|| a.outpoint.cmp(&b.outpoint))
        });
        coins
    }

    /// The coins a spend may pick: confirmed and not frozen.
    pub fn spendable_coins(&self, labels: &Labels) -> Vec<Coin> {
        let confirmed: std::collections::HashSet<String> = self
            .lock()
            .wallet
            .list_unspent()
            .filter(|u| u.chain_position.is_confirmed())
            .map(|u| u.outpoint.to_string())
            .collect();
        self.coins(labels)
            .into_iter()
            .filter(|c| !c.frozen && confirmed.contains(&c.outpoint))
            .collect()
    }

    /// The wallet's transactions, newest first, from BDK's canonical view: a
    /// transaction the chain server dropped is no longer shown. A send nests
    /// where its change went when Fedimix recorded it (`Labels::record_sent`).
    pub fn history(&self, labels: &Labels) -> Vec<HistoryItem> {
        let inner = self.lock();
        let wallet = &inner.wallet;
        let mut items: Vec<HistoryItem> = wallet
            .transactions()
            .map(|wtx| {
                let tx = &wtx.tx_node.tx;
                let txid = wtx.tx_node.txid;
                let (sent, received) = wallet.sent_and_received(tx);
                let status = if wtx.chain_position.is_confirmed() {
                    HistoryStatus::Complete
                } else {
                    HistoryStatus::Pending
                };
                let timestamp = position_time(&wtx.chain_position);
                if sent.to_sat() == 0 {
                    let unmixed = (0..tx.output.len() as u32).all(|vout| {
                        let outpoint = bitcoin::OutPoint { txid, vout };
                        !wallet.is_mine(tx.output[vout as usize].script_pubkey.clone())
                            || labels.tag(&outpoint.to_string()) == Tag::Unmixed
                    });
                    return HistoryItem {
                        id: txid.to_string(),
                        kind: HistoryKind::Received { unmixed },
                        amount: sat(received),
                        incoming: true,
                        status,
                        timestamp,
                        txid: Some(txid.to_string()),
                        fee: None,
                        session: None,
                    };
                }
                let record = labels.sent(&txid.to_string());
                let change = record.and_then(|r| r.change.clone());
                let outputs: bitcoin::Amount = tx.output.iter().map(|o| o.value).sum();
                // What left for someone else: all outputs, less what came
                // back to this wallet and less change sent to Spending.
                let to_spending = change
                    .as_ref()
                    .filter(|c| c.to_spending)
                    .map_or(Amount::ZERO, |c| c.amount);
                let amount = sat(outputs)
                    .saturating_sub(sat(received))
                    .saturating_sub(to_spending);
                HistoryItem {
                    id: txid.to_string(),
                    kind: HistoryKind::Sent { change },
                    amount,
                    incoming: false,
                    status,
                    timestamp,
                    txid: Some(txid.to_string()),
                    fee: wallet.calculate_fee(tx).ok().map(sat),
                    session: None,
                }
            })
            .collect();
        items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
        items
    }

    /// A receive address that was never given out before, and the choice is
    /// persisted before it is returned, so a restart cannot hand it out again.
    pub fn next_receive_address(&self) -> Result<String> {
        let mut inner = self.lock();
        let address = inner.wallet.reveal_next_address(KeychainKind::External);
        inner.persist()?;
        Ok(address.address.to_string())
    }

    #[cfg(test)]
    pub(crate) fn tip(&self) -> bdk_wallet::chain::CheckPoint {
        self.lock().wallet.latest_checkpoint()
    }

    #[cfg(test)]
    pub(crate) fn public_descriptor(&self, keychain: KeychainKind) -> String {
        self.lock().wallet.public_descriptor(keychain).to_string()
    }

    #[cfg(test)]
    #[allow(deprecated)] // the test asks whether any signer exists at all
    pub(crate) fn has_signers(&self) -> bool {
        let inner = self.lock();
        [KeychainKind::External, KeychainKind::Internal]
            .into_iter()
            .any(|k| !inner.wallet.get_signers(k).signers().is_empty())
    }
}

/// Whole sats become msat; never a float.
fn sat(amount: bitcoin::Amount) -> Amount {
    Amount::from_sat(amount.to_sat())
}

/// When a transaction was confirmed, or first seen while it waits.
fn position_time(position: &ChainPosition<bdk_wallet::chain::ConfirmationBlockTime>) -> u64 {
    match position {
        ChainPosition::Confirmed { anchor, .. } => anchor.confirmation_time,
        ChainPosition::Unconfirmed {
            first_seen,
            last_seen,
        } => first_seen.or(*last_seen).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bdk_wallet::bitcoin::bip32::{DerivationPath, Xpriv, Xpub};
    use bdk_wallet::bitcoin::hashes::Hash;
    use bdk_wallet::bitcoin::secp256k1::Secp256k1;
    use bdk_wallet::bitcoin::{
        Address, Amount as BtcAmount, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
        TxOut, Txid, Witness, absolute, transaction,
    };
    use bdk_wallet::chain::spk_client::SyncResponse;
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime, TxUpdate};
    use std::str::FromStr;

    use super::*;
    use crate::api::{HistoryKind, HistoryStatus, PrivacyTag, SentChange};
    use crate::error::FedimixError;
    use crate::saving::{FakeChain, Labels};
    use crate::store::Store;

    const CONFIRMED_AT: u64 = 500;
    const SEEN_AT: u64 = 1_000;

    fn account(network: Network) -> DeviceAccount {
        let (btc, coin) = match network {
            Network::Bitcoin => (bdk_wallet::bitcoin::Network::Bitcoin, 0),
            Network::Signet => (bdk_wallet::bitcoin::Network::Signet, 1),
            Network::Regtest => (bdk_wallet::bitcoin::Network::Regtest, 1),
        };
        let secp = Secp256k1::new();
        let master = Xpriv::new_master(btc, &[7u8; 32]).unwrap();
        let path = DerivationPath::from_str(&format!("m/84'/{coin}'/0'")).unwrap();
        let xpub = Xpub::from_priv(&secp, &master.derive_priv(&secp, &path).unwrap());
        DeviceAccount {
            kind: DeviceKind::Software,
            name: "Test device".into(),
            fingerprint: master.fingerprint(&secp).to_string(),
            xpub: xpub.to_string(),
            path: format!("m/84'/{coin}'/0'"),
        }
    }

    fn db(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("fedimix.db")
    }

    fn new_wallet(dir: &tempfile::TempDir) -> SavingWallet {
        SavingWallet::create(&db(dir), account(Network::Regtest), Network::Regtest).unwrap()
    }

    fn unique_input(n: u32) -> TxIn {
        TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([n as u8 + 1; 32]),
                vout: n,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }
    }

    fn tx_paying(input: TxIn, outputs: Vec<(ScriptBuf, u64)>) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![input],
            output: outputs
                .into_iter()
                .map(|(script_pubkey, sat)| TxOut {
                    script_pubkey,
                    value: BtcAmount::from_sat(sat),
                })
                .collect(),
        }
    }

    /// Shows `tx` to the wallet, in a block at `height` or in the mempool.
    fn show(w: &SavingWallet, tx: &Transaction, height: Option<u32>) {
        let txid = tx.compute_txid();
        let mut tx_update = TxUpdate::default();
        tx_update.txs = vec![Arc::new(tx.clone())];
        let mut chain = None;
        match height {
            Some(height) => {
                let block_id = BlockId {
                    height,
                    hash: BlockHash::from_byte_array([height as u8; 32]),
                };
                chain = Some(w.tip().insert(block_id));
                tx_update.anchors = [(
                    ConfirmationBlockTime {
                        block_id,
                        confirmation_time: CONFIRMED_AT,
                    },
                    txid,
                )]
                .into();
            }
            None => tx_update.seen_ats = [(txid, SEEN_AT)].into(),
        }
        w.apply_update(Update {
            chain,
            tx_update,
            ..Default::default()
        })
        .unwrap();
    }

    fn spk(address: &str) -> ScriptBuf {
        Address::from_str(address)
            .unwrap()
            .assume_checked()
            .script_pubkey()
    }

    /// A coin of `sat` on a fresh Saving address.
    fn fund(w: &SavingWallet, n: u32, sat: u64, height: Option<u32>) -> (OutPoint, String) {
        let address = w.next_receive_address().unwrap();
        let tx = tx_paying(unique_input(n), vec![(spk(&address), sat)]);
        show(w, &tx, height);
        (
            OutPoint {
                txid: tx.compute_txid(),
                vout: 0,
            },
            address,
        )
    }

    #[test]
    fn watch_only_has_no_private_keys() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        for kind in [KeychainKind::External, KeychainKind::Internal] {
            let d = w.public_descriptor(kind);
            assert!(d.starts_with("wpkh([") && !d.contains("prv"), "{d}");
        }
        assert!(!w.has_signers());
        fund(&w, 0, 10_000, Some(1));
        w.next_receive_address().unwrap();
        drop(w);
        for name in ["fedimix.db", "fedimix.db-wal"] {
            let bytes = std::fs::read(dir.path().join(name)).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("prv"), "{name} holds a private key");
        }
        let stored = Store::open(&db(&dir)).unwrap().device_account().unwrap();
        assert_eq!(stored, Some(account(Network::Regtest)));
    }

    #[test]
    fn direct_receive_is_unmixed() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let (outpoint, address) = fund(&w, 0, 50_000, Some(1));
        let coins = w.coins(&Labels::default());
        assert_eq!(coins.len(), 1);
        let coin = &coins[0];
        assert_eq!(coin.tag, PrivacyTag::Unmixed);
        assert_eq!(coin.amount, Amount::from_sat(50_000));
        assert_eq!(coin.outpoint, outpoint.to_string());
        assert_eq!(coin.address, address);
        assert_eq!(coin.received_at, CONFIRMED_AT);
        assert_eq!(coin.source_federation, None);
        assert!(!coin.frozen);
        assert_eq!(w.balance(), Amount::from_sat(50_000));
        assert_eq!(w.in_flight(), Amount::ZERO);
    }

    #[test]
    fn labels_map_to_public_tags() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let store = Store::open(&db(&dir)).unwrap();
        let mut labels = Labels::load(&store).unwrap();
        let tags = [
            crate::saving::Tag::Anonymity { estimate: 64 },
            crate::saving::Tag::EarlyExit { estimate: 9 },
            crate::saving::Tag::SmallChange,
        ];
        let mut want = vec![];
        for (n, tag) in tags.into_iter().enumerate() {
            let (op, _) = fund(&w, n as u32, 20_000 + n as u64, Some(1));
            labels
                .set_tag(&store, &op.to_string(), tag, Some("fed".into()))
                .unwrap();
            want.push((op.to_string(), tag.to_public()));
        }
        let coins = w.coins(&labels);
        for (outpoint, tag) in want {
            let coin = coins.iter().find(|c| c.outpoint == outpoint).unwrap();
            assert_eq!(coin.tag, tag);
            assert_eq!(coin.source_federation.as_deref(), Some("fed"));
        }
    }

    #[test]
    fn frozen_is_persisted_and_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let (kept, _) = fund(&w, 0, 10_000, Some(1));
        let (frozen, _) = fund(&w, 1, 20_000, Some(1));
        let store = Store::open(&db(&dir)).unwrap();
        let mut labels = Labels::load(&store).unwrap();
        labels
            .set_frozen(&store, &frozen.to_string(), true)
            .unwrap();
        let all = w.coins(&labels);
        assert_eq!(all.len(), 2);
        assert!(
            all.iter()
                .any(|c| c.outpoint == frozen.to_string() && c.frozen)
        );
        let spendable = w.spendable_coins(&labels);
        assert_eq!(spendable.len(), 1);
        assert_eq!(spendable[0].outpoint, kept.to_string());
        // Still frozen after everything is closed and opened again.
        drop((w, store, labels));
        let store = Store::open(&db(&dir)).unwrap();
        let labels = Labels::load(&store).unwrap();
        assert!(labels.is_frozen(&frozen.to_string()));
        assert!(!labels.is_frozen(&kept.to_string()));
        let w = SavingWallet::open(&db(&dir), Network::Regtest)
            .unwrap()
            .unwrap();
        assert_eq!(w.spendable_coins(&labels).len(), 1);
        // Unfreezing lets it back in.
        let mut labels = labels;
        labels
            .set_frozen(&store, &frozen.to_string(), false)
            .unwrap();
        assert_eq!(w.spendable_coins(&labels).len(), 2);
    }

    #[test]
    fn unconfirmed_coins_are_not_spendable() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        fund(&w, 0, 10_000, Some(1));
        fund(&w, 1, 20_000, None);
        let labels = Labels::default();
        assert_eq!(w.coins(&labels).len(), 2);
        assert_eq!(w.spendable_coins(&labels).len(), 1);
        assert_eq!(w.balance(), Amount::from_sat(10_000));
        assert_eq!(w.in_flight(), Amount::from_sat(20_000));
    }

    #[test]
    fn a_new_address_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let a = w.next_receive_address().unwrap();
        let b = w.next_receive_address().unwrap();
        let c = w.next_receive_address().unwrap();
        assert!(a != b && b != c && a != c);
        assert!(a.starts_with("bcrt1q"), "{a}");
    }

    #[test]
    fn reopen_keeps_revealed_indices() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let first: Vec<String> = (0..3).map(|_| w.next_receive_address().unwrap()).collect();
        drop(w);
        let reopened = SavingWallet::open(&db(&dir), Network::Regtest)
            .unwrap()
            .unwrap();
        let next = reopened.next_receive_address().unwrap();
        assert!(!first.contains(&next));
        // The reference: a fresh wallet on the same account, four in a row.
        let other = tempfile::tempdir().unwrap();
        let reference = new_wallet(&other);
        let four: Vec<String> = (0..4)
            .map(|_| reference.next_receive_address().unwrap())
            .collect();
        assert_eq!(first, four[..3]);
        assert_eq!(next, four[3]);
    }

    #[test]
    fn open_without_setup_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            SavingWallet::open(&db(&dir), Network::Regtest)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn creating_twice_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _w = new_wallet(&dir);
        let again = SavingWallet::create(&db(&dir), account(Network::Regtest), Network::Regtest);
        assert!(matches!(again, Err(FedimixError::NotSetUp(_))), "{again:?}");
    }

    #[test]
    fn wrong_network_descriptor_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        drop(SavingWallet::create(&db(&dir), account(Network::Signet), Network::Signet).unwrap());
        // Same kind of keys (test networks), another network: BDK's check.
        let err = SavingWallet::open(&db(&dir), Network::Regtest)
            .err()
            .unwrap();
        assert_eq!(
            err,
            FedimixError::WrongNetwork {
                folder: "signet".into(),
                wanted: "regtest".into()
            }
        );
        assert!(err.to_string().contains("signet"), "{err}");
        // Main network against test keys: caught before BDK is asked.
        let err = SavingWallet::open(&db(&dir), Network::Bitcoin)
            .err()
            .unwrap();
        assert!(matches!(err, FedimixError::NotSetUp(_)), "{err:?}");
        // And an account made for the main network cannot start a signet wallet.
        let other = tempfile::tempdir().unwrap();
        let err = SavingWallet::create(&db(&other), account(Network::Bitcoin), Network::Signet)
            .err()
            .unwrap();
        assert!(matches!(err, FedimixError::NotSetUp(_)), "{err:?}");
        // The refused create stored nothing.
        assert!(
            SavingWallet::open(&db(&other), Network::Signet)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn history_nests_change_under_the_send() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let (coin, _) = fund(&w, 0, 100_000, Some(1));
        let funding_txid = coin.txid;
        // Send 30k to someone, 60k change to the federation, 10k fee.
        let someone = ScriptBuf::from_bytes(vec![0x51]);
        let federation = ScriptBuf::from_bytes(vec![0x52]);
        let send = tx_paying(
            TxIn {
                previous_output: coin,
                ..unique_input(9)
            },
            vec![(someone, 30_000), (federation, 60_000)],
        );
        show(&w, &send, None);
        let store = Store::open(&db(&dir)).unwrap();
        let mut labels = Labels::load(&store).unwrap();

        // Before Fedimix records it, the send has no change to show.
        let history = w.history(&labels);
        assert_eq!(history.len(), 2);
        assert!(matches!(
            history[0].kind,
            HistoryKind::Sent { change: None }
        ));

        let change = SentChange {
            amount: Amount::from_sat(60_000),
            to_spending: true,
        };
        labels
            .record_sent(
                &store,
                &send.compute_txid().to_string(),
                Some(change.clone()),
            )
            .unwrap();
        let history = w.history(&labels);
        assert_eq!(history.len(), 2);
        let sent = &history[0];
        assert_eq!(
            sent.kind,
            HistoryKind::Sent {
                change: Some(change)
            }
        );
        assert_eq!(sent.amount, Amount::from_sat(30_000));
        assert_eq!(sent.fee, Some(Amount::from_sat(10_000)));
        assert!(!sent.incoming);
        assert_eq!(sent.status, HistoryStatus::Pending);
        assert_eq!(sent.txid, Some(send.compute_txid().to_string()));
        assert_eq!(sent.timestamp, SEEN_AT);
        let received = &history[1];
        assert_eq!(received.kind, HistoryKind::Received { unmixed: true });
        assert_eq!(received.amount, Amount::from_sat(100_000));
        assert!(received.incoming);
        assert_eq!(received.status, HistoryStatus::Complete);
        assert_eq!(received.txid, Some(funding_txid.to_string()));

        // The record survives a restart.
        drop((w, labels, store));
        let store = Store::open(&db(&dir)).unwrap();
        let labels = Labels::load(&store).unwrap();
        assert!(labels.sent(&send.compute_txid().to_string()).is_some());
    }

    #[test]
    fn small_change_stays_in_the_amount_of_the_send() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let (coin, _) = fund(&w, 0, 100_000, Some(1));
        let own_change = spk(&w.next_receive_address().unwrap());
        let send = tx_paying(
            TxIn {
                previous_output: coin,
                ..unique_input(9)
            },
            vec![
                (ScriptBuf::from_bytes(vec![0x51]), 95_000),
                (own_change, 3_000),
            ],
        );
        show(&w, &send, Some(2));
        let history = w.history(&Labels::default());
        assert_eq!(history[0].amount, Amount::from_sat(95_000));
        assert_eq!(history[0].fee, Some(Amount::from_sat(2_000)));
        assert_eq!(history[0].status, HistoryStatus::Complete);
    }

    #[test]
    fn dropped_unconfirmed_tx_leaves_history() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        fund(&w, 0, 10_000, Some(1));
        let (pending, _) = fund(&w, 1, 20_000, None);
        let labels = Labels::default();
        let history = w.history(&labels);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].status, HistoryStatus::Pending);
        assert_eq!(w.in_flight(), Amount::from_sat(20_000));

        // The server no longer knows it: the sync says it was evicted.
        let chain = FakeChain::new();
        let mut response = SyncResponse::default();
        response.tx_update.evicted_ats = [(pending.txid, SEEN_AT + 1_000)].into();
        chain.push_sync(response);
        let summary = futures::executor::block_on(w.sync(&chain)).unwrap();
        assert!(!summary.full_scan);
        assert!(summary.changed);

        let history = w.history(&labels);
        assert_eq!(history.len(), 1, "{history:?}");
        assert_eq!(history[0].status, HistoryStatus::Complete);
        assert_eq!(w.in_flight(), Amount::ZERO);
        assert_eq!(w.coins(&labels).len(), 1);
        // The eviction was persisted: still gone after a restart.
        drop(w);
        let w = SavingWallet::open(&db(&dir), Network::Regtest)
            .unwrap()
            .unwrap();
        assert_eq!(w.history(&labels).len(), 1);
    }

    #[test]
    fn first_sync_is_a_full_scan_then_routine() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let chain = FakeChain::new();
        let tip = w.tip().insert(BlockId {
            height: 3,
            hash: BlockHash::from_byte_array([3; 32]),
        });
        chain.push_full_scan(bdk_wallet::chain::spk_client::FullScanResponse {
            chain_update: Some(tip),
            ..Default::default()
        });
        let first = futures::executor::block_on(w.sync(&chain)).unwrap();
        assert!(first.full_scan);
        let second = futures::executor::block_on(w.sync(&chain)).unwrap();
        assert!(!second.full_scan);
        assert_eq!(chain.full_scans_made(), 1);
        assert_eq!(chain.scans_made(), 2);
    }

    #[test]
    fn offline_sync_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        fund(&w, 0, 10_000, Some(1));
        let chain = FakeChain::new();
        chain.set_offline(true);
        let err = futures::executor::block_on(w.sync(&chain)).unwrap_err();
        assert_eq!(err, FedimixError::Offline);
        assert_eq!(w.balance(), Amount::from_sat(10_000));
    }

    /// The wallet's connection and the store's share `fedimix.db`: writes from
    /// both at once wait for each other (WAL, busy timeout) and none fails.
    #[test]
    fn two_connections_share_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        let path = db(&dir);
        let store_thread = std::thread::spawn(move || {
            let store = Store::open(&path).unwrap();
            let mut labels = Labels::load(&store).unwrap();
            for n in 0..150 {
                labels
                    .set_frozen(&store, &format!("{n:064}:0"), n % 2 == 0)
                    .unwrap();
                store.set_cursor("busy", n).unwrap();
            }
        });
        let wallet = Arc::clone(&w);
        let wallet_thread = std::thread::spawn(move || {
            (0..150)
                .map(|_| wallet.next_receive_address().unwrap())
                .collect::<Vec<_>>()
        });
        store_thread.join().unwrap();
        let addresses = wallet_thread.join().unwrap();
        let unique: std::collections::HashSet<_> = addresses.iter().collect();
        assert_eq!(unique.len(), 150);
        drop(w);
        let store = Store::open(&db(&dir)).unwrap();
        assert!(
            Labels::load(&store)
                .unwrap()
                .is_frozen(&format!("{:064}:0", 148))
        );
        let again = SavingWallet::open(&db(&dir), Network::Regtest)
            .unwrap()
            .unwrap();
        // 150 revealed and persisted: the next one is the 151st.
        let reference = new_wallet(&tempfile::tempdir().unwrap());
        let expected = (0..151)
            .map(|_| reference.next_receive_address().unwrap())
            .last();
        assert_eq!(again.next_receive_address().ok(), expected);
    }
}
