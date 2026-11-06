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
//! applies the answer under the lock again, on a blocking thread, as that write
//! waits for a disk and for the store's connection.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bdk_wallet::Wallet;
use bdk_wallet::bitcoin::bip32::{DerivationPath, Fingerprint, Xpub};
use bdk_wallet::bitcoin::constants::COINBASE_MATURITY;
use bdk_wallet::bitcoin::psbt::Psbt;
use bdk_wallet::bitcoin::{
    self, Address, FeeRate, NetworkKind, OutPoint, ScriptBuf, Transaction, Txid,
};
use bdk_wallet::chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_wallet::chain::{BlockId, ChainPosition};
use bdk_wallet::error::CreateTxError;
use bdk_wallet::rusqlite::{Connection, Transaction as DbTransaction, TransactionBehavior};
use bdk_wallet::{
    Balance, ChangeSet, CreateWithPersistError, KeychainKind, LoadError, LoadMismatch,
    LoadWithPersistError, PersistedWallet, SignOptions, Update, WalletPersister,
};

use crate::api::{Amount, Coin, DeviceKind, HistoryItem, HistoryKind, HistoryStatus, Network};
use crate::error::{FedimixError, Result};
use crate::saving::{ChainClient, Labels, Tag};
use crate::store::{Store, db_err};

/// How far a first scan looks past the last used address. Fedimix reveals an
/// address per receive, so a restored wallet may have long runs of unused ones.
const FULL_SCAN_STOP_GAP: usize = 100;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const ALREADY_SET_UP: &str = "Saving is already set up.";

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
    /// The balance, the chain tip or the set of transactions moved, and it is
    /// persisted. A pending transaction merely seen again is no change.
    pub changed: bool,
}

/// What the chain said, not yet applied. Waiting for it can be cancelled.
pub(crate) struct Fetched {
    full_scan: bool,
    update: Update,
}

/// One moment of the wallet, read under one lock.
#[derive(Debug, Clone, PartialEq)]
pub struct SavingSnapshot {
    pub balance: Amount,
    pub in_flight: Amount,
    pub coins: Vec<Coin>,
    pub history: Vec<HistoryItem>,
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
        let mut tx = DbTransaction::new(&mut persister.0, TransactionBehavior::Immediate)?;
        let changeset = <DbTransaction<'_> as WalletPersister>::initialize(&mut tx)?;
        tx.commit()?;
        Ok(changeset)
    }

    fn persist(
        persister: &mut Self,
        changeset: &ChangeSet,
    ) -> std::result::Result<(), Self::Error> {
        let mut tx = DbTransaction::new(&mut persister.0, TransactionBehavior::Immediate)?;
        <DbTransaction<'_> as WalletPersister>::persist(&mut tx, changeset)?;
        tx.commit()
    }
}

struct Inner {
    wallet: PersistedWallet<Db>,
    db: Db,
    /// A change that a failed write left unsaved.
    unsaved: bool,
}

impl Inner {
    /// Writes what the wallet has staged.
    fn persist(&mut self) -> Result<bool> {
        self.wallet.persist(&mut self.db).map_err(db_err)
    }

    /// A wallet that never heard from a chain server knows only genesis.
    fn first_scan_done(&self) -> bool {
        self.wallet.latest_checkpoint().height() != 0
    }

    /// What a sync can change that anyone would notice.
    fn view(&self) -> (Balance, BlockId, BTreeSet<Txid>) {
        (
            self.wallet.balance(),
            self.wallet.latest_checkpoint().block_id(),
            self.wallet
                .transactions()
                .map(|wtx| wtx.tx_node.txid)
                .collect(),
        )
    }

    /// The outpoints of coins past coinbase maturity, 100 confirmations, as
    /// `Wallet::balance` counts it. Any coin not from a coinbase is mature.
    fn mature_coins(&self) -> HashSet<String> {
        let tip = self.wallet.latest_checkpoint().height();
        let graph = self.wallet.tx_graph();
        self.wallet
            .list_unspent()
            .filter(|utxo| {
                let coinbase = graph
                    .get_tx(utxo.outpoint.txid)
                    .is_some_and(|tx| tx.is_coinbase());
                let confirmed_at = utxo.chain_position.confirmation_height_upper_bound();
                !coinbase
                    || confirmed_at
                        .is_some_and(|at| tip.saturating_sub(at) + 1 >= COINBASE_MATURITY)
            })
            .map(|utxo| utxo.outpoint.to_string())
            .collect()
    }

    fn coins(&self, network: Network, labels: &Labels) -> Vec<Coin> {
        let mut coins: Vec<Coin> = self
            .wallet
            .list_unspent()
            .map(|utxo| {
                let outpoint = utxo.outpoint.to_string();
                let address = Address::from_script(&utxo.txout.script_pubkey, btc_network(network))
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                Coin {
                    amount: sat(utxo.txout.value),
                    tag: labels.coin_tag(&outpoint, &address).to_public(),
                    received_at: position_time(&utxo.chain_position),
                    source_federation: labels.coin_source(&outpoint, &address),
                    frozen: labels.is_frozen(&outpoint),
                    address,
                    confirmed: utxo.chain_position.is_confirmed(),
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

    /// One row: what `tx` did to this wallet.
    fn item(
        &self,
        tx: &Transaction,
        status: HistoryStatus,
        timestamp: u64,
        labels: &Labels,
    ) -> HistoryItem {
        let wallet = &self.wallet;
        let txid = tx.compute_txid();
        let (sent, received) = wallet.sent_and_received(tx);
        if sent.to_sat() == 0 {
            let address = |output: &bitcoin::TxOut| {
                Address::from_script(&output.script_pubkey, wallet.network())
                    .map(|a| a.to_string())
                    .unwrap_or_default()
            };
            // An exit pays its address exactly the denomination.
            let exit = tx.output.iter().find(|output| {
                wallet.is_mine(output.script_pubkey.clone())
                    && labels.exit_at(&address(output)).is_some()
            });
            let unmixed = tx.output.iter().zip(0u32..).all(|(output, vout)| {
                let outpoint = OutPoint { txid, vout };
                !wallet.is_mine(output.script_pubkey.clone())
                    || labels.coin_tag(&outpoint.to_string(), &address(output)) == Tag::Unmixed
            });
            let kind = match exit {
                Some(output) => HistoryKind::FromSpending {
                    denomination: sat(output.value),
                },
                None => HistoryKind::Received { unmixed },
            };
            return HistoryItem {
                id: txid.to_string(),
                kind,
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
        let refill = record.is_some_and(|r| r.refill);
        let change = record.and_then(|r| r.change.clone()).filter(|_| !refill);
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
            kind: if refill {
                HistoryKind::Refill
            } else {
                HistoryKind::Sent { change }
            },
            amount,
            incoming: false,
            status,
            timestamp,
            txid: Some(txid.to_string()),
            fee: wallet.calculate_fee(tx).ok().map(sat),
            session: None,
        }
    }

    fn history(&self, labels: &Labels) -> Vec<HistoryItem> {
        let mut items: Vec<HistoryItem> = self
            .wallet
            .transactions()
            .map(|wtx| {
                let status = if wtx.chain_position.is_confirmed() {
                    HistoryStatus::Complete
                } else {
                    HistoryStatus::Pending
                };
                let timestamp = position_time(&wtx.chain_position);
                self.item(&wtx.tx_node.tx, status, timestamp, labels)
            })
            .collect();
        sort_history(&mut items);
        items
    }
}

/// Newest first.
pub(crate) fn sort_history(items: &mut [HistoryItem]) {
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
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
        "Fedimix could not read {what} from the wallet you chose. Set up Saving again."
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
            "The Saving account is for {has}, but Fedimix is running on {wants}."
        )));
    }
    let origin = format!("[{fingerprint}/{path}]{xpub}");
    Ok((format!("wpkh({origin}/0/*)"), format!("wpkh({origin}/1/*)")))
}

impl SavingWallet {
    /// Stores the account and creates the wallet. Refused when another account
    /// is stored; the same one carries on, as an earlier attempt may have
    /// stopped between the two writes. Nothing is written for an account that
    /// does not fit `network`.
    pub fn create(
        store_path: &Path,
        account: DeviceAccount,
        network: Network,
    ) -> Result<SavingWallet> {
        descriptors(&account, network)?;
        let store = Store::open(store_path)?;
        if !store.set_device_account(&account)? && store.device_account()? != Some(account.clone())
        {
            return Err(FedimixError::NotSetUp(ALREADY_SET_UP.into()));
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
                    "The Saving wallet in this data folder does not match the account it was set up with: {e}."
                )));
            }
        };
        Ok(SavingWallet {
            network,
            inner: Mutex::new(Inner {
                wallet,
                db,
                unsaved: false,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Asks the chain what changed and applies the answer. The first sync of a
    /// wallet is a full scan (`stop_gap` 100); later ones look only at the
    /// scripts already revealed. The lock is not held while waiting.
    pub async fn sync(self: &Arc<Self>, chain: &dyn ChainClient) -> Result<SyncSummary> {
        let fetched = self.fetch(chain).await?;
        Arc::clone(self).apply(fetched).await
    }

    /// The asking half of [`Self::sync`]. Dropping it leaves nothing behind.
    pub(crate) async fn fetch(&self, chain: &dyn ChainClient) -> Result<Fetched> {
        enum Request {
            Full(FullScanRequest<KeychainKind>),
            Routine(SyncRequest<(KeychainKind, u32)>),
        }
        let request = {
            let inner = self.lock();
            if inner.first_scan_done() {
                Request::Routine(inner.wallet.start_sync_with_revealed_spks().build())
            } else {
                Request::Full(inner.wallet.start_full_scan().build())
            }
        };
        Ok(match request {
            Request::Full(request) => Fetched {
                full_scan: true,
                update: chain.full_scan(request, FULL_SCAN_STOP_GAP).await?.into(),
            },
            Request::Routine(request) => Fetched {
                full_scan: false,
                update: chain.sync(request).await?.into(),
            },
        })
    }

    /// The applying half of [`Self::sync`], on a blocking thread. It runs to
    /// the end once started, so wait for it before letting go of the folder.
    pub(crate) async fn apply(self: Arc<Self>, fetched: Fetched) -> Result<SyncSummary> {
        let Fetched { full_scan, update } = fetched;
        let changed = tokio::task::spawn_blocking(move || self.apply_update(update))
            .await
            .map_err(|_| {
                FedimixError::Internal("Fedimix could not save what the chain server said.".into())
            })??;
        Ok(SyncSummary { full_scan, changed })
    }

    /// Whether the first scan has finished. Until then the address index knows
    /// nothing of what the device used before.
    pub fn first_scan_done(&self) -> bool {
        self.lock().first_scan_done()
    }

    /// Applies `update`, and persists when it told the wallet something. What
    /// else BDK staged is `last_seen` of a pending transaction, sent on every
    /// sync; it stays staged and goes with the next real change, so a quiet
    /// wallet does not write to disk each time. A change whose write failed
    /// counts until a later call has written it.
    pub(crate) fn apply_update(&self, update: Update) -> Result<bool> {
        let mut inner = self.lock();
        let before = inner.view();
        inner.wallet.apply_update(update).map_err(|_| {
            FedimixError::Internal(
                "The chain server is on a different chain from this wallet. Check the server in Settings."
                    .into(),
            )
        })?;
        let changed = inner.unsaved || inner.view() != before;
        if changed {
            inner.unsaved = true;
            inner.persist()?;
            inner.unsaved = false;
        }
        Ok(changed)
    }

    /// Balance, coins and history of one moment.
    pub fn snapshot(&self, labels: &Labels) -> SavingSnapshot {
        let inner = self.lock();
        let balance = inner.wallet.balance();
        SavingSnapshot {
            balance: sat(balance.confirmed + balance.trusted_pending),
            in_flight: sat(balance.untrusted_pending),
            coins: inner.coins(self.network, labels),
            history: inner.history(labels),
        }
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
        self.lock().coins(self.network, labels)
    }

    /// The coins a spend may pick: confirmed, mature and not frozen.
    pub fn spendable_coins(&self, labels: &Labels) -> Vec<Coin> {
        let inner = self.lock();
        let mature = inner.mature_coins();
        let mut coins = inner.coins(self.network, labels);
        coins.retain(|coin| !coin.frozen && coin.confirmed && mature.contains(&coin.outpoint));
        coins
    }

    /// Whether `outpoint` (`txid:vout`) is an unspent coin of this wallet.
    pub fn has_coin(&self, outpoint: &str) -> bool {
        OutPoint::from_str(outpoint).is_ok_and(|outpoint| {
            let inner = self.lock();
            inner.wallet.get_utxo(outpoint).is_some()
        })
    }

    /// The wallet's transactions, newest first, from BDK's canonical view: a
    /// transaction the chain server dropped is no longer shown. A send nests
    /// where its change went when Fedimix recorded it (`Labels::record_sent`).
    pub fn history(&self, labels: &Labels) -> Vec<HistoryItem> {
        self.lock().history(labels)
    }

    /// A receive address that was never given out before, and the choice is
    /// persisted before it is returned, so a restart cannot hand it out again.
    pub fn next_receive_address(&self) -> Result<String> {
        let mut inner = self.lock();
        let address = inner.wallet.reveal_next_address(KeychainKind::External);
        inner.persist()?;
        Ok(address.address.to_string())
    }

    /// What a send may pick from: [`Self::spendable_coins`] less what a stored
    /// transaction has signed away.
    pub(crate) fn usable_coins(&self, labels: &Labels, locked: &HashSet<OutPoint>) -> Vec<Coin> {
        let mut coins = self.spendable_coins(labels);
        coins.retain(|coin| {
            OutPoint::from_str(&coin.outpoint).is_ok_and(|outpoint| !locked.contains(&outpoint))
        });
        coins
    }

    /// Builds an unsigned send from the usable coins. Change goes to
    /// `plan.change_to`, or to the internal keychain, so that small change
    /// counts as the wallet's own and not as money on its way from someone.
    /// An internal address this uses is revealed but not yet written: call
    /// [`Self::save`] before the transaction can reach anyone.
    pub(crate) fn build_send(&self, labels: &Labels, plan: &SendPlan) -> Result<BuiltSend> {
        let usable = self.usable_coins(labels, &plan.locked);
        let available = usable.iter().fold(Amount::ZERO, |total, coin| Amount {
            msat: total.msat.saturating_add(coin.amount.msat),
        });
        let mut inner = self.lock();
        let held_back: Vec<OutPoint> = inner
            .wallet
            .list_unspent()
            .map(|utxo| utxo.outpoint)
            .filter(|outpoint| {
                let outpoint = outpoint.to_string();
                !usable.iter().any(|coin| coin.outpoint == outpoint)
            })
            .collect();
        let mut builder = inner.wallet.build_tx();
        builder.fee_rate(plan.fee_rate).unspendable(held_back);
        match plan.amount {
            Some(amount) => {
                builder.add_recipient(plan.to.clone(), amount);
                if !plan.must_spend.is_empty() {
                    let added = builder.add_utxos(&plan.must_spend);
                    added.map_err(|_| FedimixError::PreviewExpired)?;
                }
                if plan.only_must_spend {
                    builder.manually_selected_only();
                }
                if let Some(change_to) = &plan.change_to {
                    builder.drain_to(change_to.clone());
                }
            }
            None => {
                builder.drain_wallet().drain_to(plan.to.clone());
            }
        }
        let psbt = builder.finish().map_err(|e| build_error(&e, available))?;
        let fee = psbt.fee().map_err(|_| {
            FedimixError::Internal("Fedimix could not work out this transaction's fee.".into())
        })?;
        let change = psbt
            .unsigned_tx
            .output
            .iter()
            .zip(0u32..)
            .find(|(output, _)| output.script_pubkey != plan.to)
            .map(|(output, vout)| (vout, output.value));
        Ok(BuiltSend { psbt, fee, change })
    }

    /// Builds a refill: the coins of `plan.must_spend` first, then the usable
    /// coins from the smallest up, one at a time, until what the one output
    /// delivers after the fee covers `plan.gap` and exceeds `plan.floor`. BDK's
    /// coin selection never runs, so nothing here is random, and there is no
    /// change. Also returns the coins in the order they were taken.
    pub(crate) fn build_refill(
        &self,
        labels: &Labels,
        plan: &RefillPlan,
    ) -> Result<(BuiltSend, Vec<OutPoint>)> {
        let mut usable = self.usable_coins(labels, &plan.locked);
        usable.sort_by(|a, b| {
            a.amount
                .cmp(&b.amount)
                .then_with(|| a.outpoint.cmp(&b.outpoint))
        });
        let mut smallest = usable
            .iter()
            .filter_map(|coin| OutPoint::from_str(&coin.outpoint).ok())
            .filter(|outpoint| !plan.must_spend.contains(outpoint));
        let mut chosen = plan.must_spend.clone();
        // The most an attempt delivered, for the refusal.
        let mut most = Amount::ZERO;
        loop {
            if !chosen.is_empty()
                && let Some(built) = self.drain(&chosen, plan)?
            {
                let delivered = built.psbt.unsigned_tx.output.first().map(|o| sat(o.value));
                let delivered = delivered.unwrap_or(Amount::ZERO);
                if delivered >= plan.gap && delivered > plan.floor {
                    return Ok((built, chosen));
                }
                most = most.max(delivered);
            }
            match smallest.next() {
                Some(outpoint) => chosen.push(outpoint),
                None => {
                    let floor = plan.floor.checked_add(Amount::from_sat(1));
                    return Err(FedimixError::InsufficientFunds {
                        available: most,
                        needed: plan.gap.max(floor.unwrap_or(plan.floor)),
                    });
                }
            }
        }
    }

    /// All of `inputs` to `plan.to`, less the fee, in one output. `None` when
    /// they cannot pay the fee and leave an output worth having.
    fn drain(&self, inputs: &[OutPoint], plan: &RefillPlan) -> Result<Option<BuiltSend>> {
        let mut inner = self.lock();
        let mut builder = inner.wallet.build_tx();
        builder
            .fee_rate(plan.fee_rate)
            .manually_selected_only()
            .drain_to(plan.to.clone());
        builder
            .add_utxos(inputs)
            .map_err(|_| FedimixError::PreviewExpired)?;
        let psbt = match builder.finish() {
            Ok(psbt) => psbt,
            Err(CreateTxError::CoinSelection(_)) => return Ok(None),
            Err(e) => return Err(build_error(&e, Amount::ZERO)),
        };
        let fee = psbt.fee().map_err(|_| {
            FedimixError::Internal("Fedimix could not work out this transaction's fee.".into())
        })?;
        Ok(Some(BuiltSend {
            psbt,
            fee,
            change: None,
        }))
    }

    /// The transaction a device signed. Makes no check of what it pays.
    pub(crate) fn finalize(&self, mut psbt: Psbt) -> Result<Transaction> {
        let inner = self.lock();
        let finalized = inner
            .wallet
            .finalize_psbt(&mut psbt, SignOptions::default());
        if !finalized.unwrap_or(false) {
            return Err(FedimixError::Internal(
                "Your device did not sign this transaction completely.".into(),
            ));
        }
        // The fee was the one reviewed, whatever rate that makes it.
        Ok(psbt.extract_tx_unchecked_fee_rate())
    }

    /// Puts a transaction that reached the server into the wallet at once, as
    /// seen now, so its coins do not look spendable until the next sync. Later
    /// than any eviction: BDK treats an equal time as evicted.
    pub(crate) fn insert_sent(&self, tx: &Transaction) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let mut inner = self.lock();
        let evicted = inner.wallet.tx_graph().get_last_evicted(tx.compute_txid());
        let seen_at = evicted.map_or(now, |evicted| now.max(evicted + 1));
        inner.wallet.apply_unconfirmed_txs([(tx.clone(), seen_at)]);
        inner.unsaved = true;
        inner.persist()?;
        inner.unsaved = false;
        Ok(())
    }

    /// Writes what the wallet has staged, such as a revealed change address.
    pub(crate) fn save(&self) -> Result<()> {
        self.lock().persist().map(|_| ())
    }

    pub(crate) fn tx_position(&self, txid: Txid) -> TxPosition {
        let inner = self.lock();
        let tip = inner.wallet.latest_checkpoint().height();
        let position = inner.wallet.get_tx(txid).map(|tx| tx.chain_position);
        match position {
            Some(position) => match position.confirmation_height_upper_bound() {
                Some(height) if position.is_confirmed() => TxPosition::Confirmed {
                    depth: tip.saturating_sub(height) + 1,
                },
                _ => TxPosition::Pending,
            },
            None => TxPosition::Absent,
        }
    }

    /// The deepest confirmed transaction that spends a coin of `tx` and is not
    /// `tx`, with its depth: the only proof that `tx` can never confirm. `Absent`
    /// says no such thing, as a transaction merely forgotten is absent too.
    pub(crate) fn conflict(&self, tx: &Transaction) -> Option<(Txid, u32)> {
        let inputs: HashSet<OutPoint> = tx.input.iter().map(|i| i.previous_output).collect();
        let txid = tx.compute_txid();
        let inner = self.lock();
        let tip = inner.wallet.latest_checkpoint().height();
        inner
            .wallet
            .transactions()
            .filter(|other| other.tx_node.txid != txid)
            .filter(|other| {
                let spent = &other.tx_node.tx.input;
                spent.iter().any(|i| inputs.contains(&i.previous_output))
            })
            .filter(|other| other.chain_position.is_confirmed())
            .filter_map(|other| {
                let height = other.chain_position.confirmation_height_upper_bound()?;
                Some((other.tx_node.txid, tip.saturating_sub(height) + 1))
            })
            .max_by_key(|(_, depth)| *depth)
    }

    /// The history row of a stored send that BDK's view no longer lists.
    pub(crate) fn signed_item(
        &self,
        tx: &Transaction,
        status: HistoryStatus,
        at: u64,
        labels: &Labels,
    ) -> HistoryItem {
        self.lock().item(tx, status, at, labels)
    }

    /// Where a send went: its first output that is neither this wallet's nor
    /// `change` (an address).
    pub(crate) fn recipient(&self, tx: &Transaction, change: Option<&str>) -> Option<String> {
        let inner = self.lock();
        tx.output
            .iter()
            .filter(|output| !inner.wallet.is_mine(output.script_pubkey.clone()))
            .filter_map(|output| {
                Address::from_script(&output.script_pubkey, btc_network(self.network)).ok()
            })
            .map(|address| address.to_string())
            .find(|address| Some(address.as_str()) != change)
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

/// What to build. `amount: None` spends every usable coin, with no change.
#[derive(Debug)]
pub(crate) struct SendPlan {
    pub to: ScriptBuf,
    pub amount: Option<bitcoin::Amount>,
    pub fee_rate: FeeRate,
    /// Where change goes; `None` is this wallet's internal keychain.
    pub change_to: Option<ScriptBuf>,
    /// Coins signed away by a stored transaction, whatever the chain says now.
    pub locked: HashSet<OutPoint>,
    /// Coins this send must spend.
    pub must_spend: Vec<OutPoint>,
    /// Spend `must_spend` and nothing else.
    pub only_must_spend: bool,
}

/// What a refill pays, and how much must arrive.
#[derive(Debug)]
pub(crate) struct RefillPlan {
    /// The federation's deposit address.
    pub to: ScriptBuf,
    pub fee_rate: FeeRate,
    /// Coins signed away by a stored transaction, whatever the chain says now.
    pub locked: HashSet<OutPoint>,
    /// Coins it must spend, whatever their size: they go first.
    pub must_spend: Vec<OutPoint>,
    /// What must arrive at least.
    pub gap: Amount,
    /// What must arrive more than.
    pub floor: Amount,
}

/// An unsigned send.
#[derive(Debug)]
pub(crate) struct BuiltSend {
    pub psbt: Psbt,
    pub fee: bitcoin::Amount,
    /// The output that is not the payment, if there is one.
    pub change: Option<(u32, bitcoin::Amount)>,
}

/// Where the wallet's canonical view has a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxPosition {
    /// In a block, `depth` of them counting its own.
    Confirmed {
        depth: u32,
    },
    Pending,
    Absent,
}

fn build_error(error: &CreateTxError, available: Amount) -> FedimixError {
    match error {
        CreateTxError::CoinSelection(short) => FedimixError::InsufficientFunds {
            available,
            needed: sat(short.needed),
        },
        CreateTxError::OutputBelowDustLimit(_) => {
            FedimixError::InvalidAmount("This amount is too small to send on chain.".into())
        }
        other => FedimixError::Internal(format!("Fedimix could not build this send: {other}.")),
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
    use bdk_wallet::bitcoin::bip32::{DerivationPath, Xpriv, Xpub};
    use bdk_wallet::bitcoin::hashes::Hash;
    use bdk_wallet::bitcoin::secp256k1::Secp256k1;
    use bdk_wallet::bitcoin::{
        Amount as BtcAmount, BlockHash, Sequence, TxIn, TxOut, Witness, absolute, transaction,
    };
    use bdk_wallet::chain::spk_client::SyncResponse;
    use bdk_wallet::chain::{CheckPoint, ConfirmationBlockTime, TxUpdate};
    use std::str::FromStr;

    use super::*;
    use crate::api::{HistoryKind, HistoryStatus, PrivacyTag, SentChange};
    use crate::error::FedimixError;
    use crate::saving::{FakeChain, Labels, Signer, SoftSigner, Tag};
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

    /// Records `address` as the exit of a 100_000 sat item of federation "fed".
    fn label_exit(store: &Store, n: u32, address: &str, estimate: u32, early: bool) {
        use crate::scheduler::Origin;
        use crate::store::{ExitItem, ExitStart, ExitState, Watch, WatchKind};
        let id = format!("exit-{n}");
        let denomination = Amount::from_sat(100_000);
        store
            .add_exit_items(&[ExitItem {
                id: id.clone(),
                federation_id: "fed".into(),
                denomination,
                origin: Origin { session: 1, at: 1 },
                created_at: 1,
                state: ExitState::Queued,
                intent: None,
                address: None,
                txid: None,
                fee: None,
                estimate: None,
                early: false,
            }])
            .unwrap();
        let intent = format!("intent-{n}");
        store
            .begin_exit(&ExitStart {
                item_id: id,
                address: address.into(),
                estimate,
                early,
                fee: Amount::from_sat(300),
                row: HistoryItem {
                    id: intent.clone(),
                    kind: HistoryKind::ToSaving { denomination },
                    amount: denomination,
                    incoming: false,
                    status: HistoryStatus::Pending,
                    timestamp: 1,
                    txid: None,
                    fee: None,
                    session: None,
                },
                watch: Watch {
                    id: intent,
                    federation_id: "fed".into(),
                    kind: WatchKind::Exit,
                    created_at: 1,
                },
            })
            .unwrap();
    }

    /// Pays `sat` to `address` in a block.
    fn pay(w: &SavingWallet, n: u32, address: &str, sat: u64) -> OutPoint {
        let tx = tx_paying(unique_input(n), vec![(spk(address), sat)]);
        show(w, &tx, Some(1));
        OutPoint {
            txid: tx.compute_txid(),
            vout: 0,
        }
    }

    fn coin_at<'a>(coins: &'a [Coin], outpoint: &OutPoint) -> &'a Coin {
        let outpoint = outpoint.to_string();
        coins.iter().find(|c| c.outpoint == outpoint).unwrap()
    }

    /// The label is read by address, so it holds for a coin that came before
    /// the label was read and for one that came after.
    #[test]
    fn an_exit_coin_is_tagged_by_its_address() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let store = Store::open(&db(&dir)).unwrap();
        let address = w.next_receive_address().unwrap();
        let early = pay(&w, 0, &address, 100_000);
        label_exit(&store, 0, &address, 42, false);
        let late_address = w.next_receive_address().unwrap();
        label_exit(&store, 1, &late_address, 43, false);
        let mut labels = Labels::load(&store).unwrap();
        let late = pay(&w, 1, &late_address, 100_000);

        let coins = w.coins(&labels);
        for (outpoint, estimate) in [(&early, 42), (&late, 43)] {
            let coin = coin_at(&coins, outpoint);
            assert_eq!(coin.tag, PrivacyTag::Anonymity { estimate });
            assert_eq!(coin.source_federation.as_deref(), Some("fed"));
        }

        // A freeze alone leaves the exit's tag; a tag on the outpoint wins.
        labels.set_frozen(&store, &early.to_string(), true).unwrap();
        labels
            .set_tag(&store, &late.to_string(), Tag::SmallChange, None)
            .unwrap();
        let coins = w.coins(&labels);
        assert_eq!(
            coin_at(&coins, &early).tag,
            PrivacyTag::Anonymity { estimate: 42 }
        );
        assert!(coin_at(&coins, &early).frozen);
        assert_eq!(coin_at(&coins, &late).tag, PrivacyTag::SmallChange);
        assert_eq!(
            coin_at(&coins, &late).source_federation.as_deref(),
            Some("fed")
        );
    }

    #[test]
    fn an_early_exit_coin_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let store = Store::open(&db(&dir)).unwrap();
        let address = w.next_receive_address().unwrap();
        label_exit(&store, 0, &address, 7, true);
        let outpoint = pay(&w, 0, &address, 100_000);
        let coins = w.coins(&Labels::load(&store).unwrap());
        let coin = coin_at(&coins, &outpoint);
        assert_eq!(coin.tag, PrivacyTag::EarlyExit { estimate: 7 });
        assert_eq!(coin.source_federation.as_deref(), Some("fed"));
    }

    #[test]
    fn an_exit_reads_from_spending_in_saving_history() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let store = Store::open(&db(&dir)).unwrap();
        let address = w.next_receive_address().unwrap();
        label_exit(&store, 0, &address, 42, false);
        let outpoint = pay(&w, 0, &address, 100_000);
        let history = w.history(&Labels::load(&store).unwrap());
        assert_eq!(history.len(), 1);
        let row = &history[0];
        assert_eq!(
            row.kind,
            HistoryKind::FromSpending {
                denomination: Amount::from_sat(100_000)
            }
        );
        assert_eq!(row.amount, Amount::from_sat(100_000));
        assert!(row.incoming);
        assert_eq!(row.status, HistoryStatus::Complete);
        assert_eq!(row.txid, Some(outpoint.txid.to_string()));
    }

    #[test]
    fn direct_receives_stay_unmixed() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let store = Store::open(&db(&dir)).unwrap();
        let exit_address = w.next_receive_address().unwrap();
        label_exit(&store, 0, &exit_address, 42, false);
        pay(&w, 0, &exit_address, 100_000);
        let (direct, _) = fund(&w, 1, 30_000, Some(2));
        let labels = Labels::load(&store).unwrap();
        let coins = w.coins(&labels);
        let coin = coin_at(&coins, &direct);
        assert_eq!(coin.tag, PrivacyTag::Unmixed);
        assert_eq!(coin.source_federation, None);
        let history = w.history(&labels);
        let row = history
            .iter()
            .find(|row| row.id == direct.txid.to_string())
            .unwrap();
        assert_eq!(row.kind, HistoryKind::Received { unmixed: true });
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
    fn creating_on_another_account_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _w = new_wallet(&dir);
        let other = DeviceAccount {
            fingerprint: "aabbccdd".into(),
            ..account(Network::Regtest)
        };
        let again = SavingWallet::create(&db(&dir), other, Network::Regtest);
        assert!(matches!(again, Err(FedimixError::NotSetUp(_))), "{again:?}");
    }

    /// The account row commits before the wallet: a retry must get past it.
    #[test]
    fn a_failed_create_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        // As after a failed attach: the store has the account, BDK nothing.
        Store::open(&db(&dir))
            .unwrap()
            .set_device_account(&account(Network::Regtest))
            .unwrap();
        let w = SavingWallet::create(&db(&dir), account(Network::Regtest), Network::Regtest);
        assert!(w.unwrap().next_receive_address().is_ok());
    }

    #[test]
    fn immature_coinbase_is_not_spendable() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let address = w.next_receive_address().unwrap();
        let coinbase = tx_paying(
            TxIn {
                previous_output: OutPoint::null(),
                ..unique_input(0)
            },
            vec![(spk(&address), 5_000_000)],
        );
        show(&w, &coinbase, Some(5));
        let labels = Labels::default();
        assert_eq!(w.coins(&labels).len(), 1, "it is yours");
        assert!(w.coins(&labels)[0].confirmed);
        assert!(w.spendable_coins(&labels).is_empty(), "not yet");
        // One block short of 100 confirmations, then the hundredth.
        let advance = |height: u32| {
            let block_id = BlockId {
                height,
                hash: BlockHash::from_byte_array([height as u8; 32]),
            };
            w.apply_update(Update {
                chain: Some(w.tip().insert(block_id)),
                ..Default::default()
            })
            .unwrap();
        };
        advance(103);
        assert!(w.spendable_coins(&labels).is_empty());
        advance(104);
        assert_eq!(w.spendable_coins(&labels).len(), 1);
    }

    #[tokio::test]
    async fn a_sync_that_learns_nothing_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        fund(&w, 0, 10_000, Some(1));
        let (pending, _) = fund(&w, 1, 20_000, None);
        let tx = w.lock().wallet.get_tx(pending.txid).unwrap().tx_node.tx;
        let chain = FakeChain::new();
        // The same transaction, seen again later: only `last_seen` moves.
        let mut response = SyncResponse::default();
        response.tx_update.txs = vec![tx];
        response.tx_update.seen_ats = [(pending.txid, SEEN_AT + 60)].into();
        chain.push_sync(response);
        assert!(!w.sync(&chain).await.unwrap().changed);
        assert!(w.lock().wallet.staged().is_some(), "held, not written");

        // A new block is news, and what was held goes with it.
        chain.push_sync(SyncResponse {
            chain_update: Some(w.tip().insert(BlockId {
                height: 2,
                hash: BlockHash::from_byte_array([2; 32]),
            })),
            ..Default::default()
        });
        assert!(w.sync(&chain).await.unwrap().changed);
        assert!(w.lock().wallet.staged().is_none());
    }

    /// A write that failed stays owed: a quiet sync after it must make it.
    #[tokio::test]
    async fn a_failed_write_is_made_by_the_next_sync() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        w.lock()
            .db
            .0
            .busy_timeout(Duration::from_millis(10))
            .unwrap();
        let block = |height: u32| Update {
            chain: Some(w.tip().insert(BlockId {
                height,
                hash: BlockHash::from_byte_array([height as u8; 32]),
            })),
            ..Default::default()
        };
        let other = Connection::open(db(&dir)).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(w.apply_update(block(1)).is_err(), "the database is locked");
        other.execute_batch("COMMIT").unwrap();

        assert!(w.sync(&FakeChain::new()).await.unwrap().changed);
        assert!(w.lock().wallet.staged().is_none(), "the write was made");
    }

    #[test]
    fn a_snapshot_agrees_with_itself() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        let (settled, _) = fund(&w, 0, 10_000, Some(1));
        let (pending, _) = fund(&w, 1, 20_000, None);
        let snapshot = w.snapshot(&Labels::default());
        assert_eq!(snapshot.balance, Amount::from_sat(10_000));
        assert_eq!(snapshot.in_flight, Amount::from_sat(20_000));
        assert_eq!(snapshot.history.len(), 2);
        let confirmed = |outpoint: OutPoint| {
            let coins = &snapshot.coins;
            coins
                .iter()
                .find(|c| c.outpoint == outpoint.to_string())
                .unwrap()
                .confirmed
        };
        assert!(confirmed(settled));
        assert!(!confirmed(pending));
    }

    #[test]
    fn another_chain_points_at_the_server_in_settings() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        fund(&w, 0, 10_000, Some(1));
        let other = CheckPoint::new(BlockId {
            height: 0,
            hash: BlockHash::from_byte_array([9; 32]),
        });
        let err = w
            .apply_update(Update {
                chain: Some(other),
                ..Default::default()
            })
            .unwrap_err();
        let FedimixError::Internal(sentence) = err else {
            panic!("{err:?}");
        };
        assert!(sentence.contains("server in Settings"), "{sentence}");
        assert!(!sentence.contains("Try again"), "{sentence}");
    }

    /// A slow write must not stop the worker that runs everything else: this
    /// runtime has one thread, and the ticks must go on during the write.
    #[tokio::test(flavor = "current_thread")]
    async fn applying_leaves_the_async_worker_free() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        let fetched = w.fetch(&FakeChain::new()).await.unwrap();
        let (locked, is_locked) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = Arc::clone(&w);
        // The write lasts until the ticks are counted, not for a time that a
        // loaded machine can outrun before the test thread gets to run.
        let held = std::thread::spawn(move || {
            let _guard = holder.lock();
            locked.send(()).unwrap();
            released.recv().unwrap();
        });
        is_locked.recv().unwrap();
        let ticks = async {
            let mut ticks = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks += 1;
                if ticks == 5 {
                    return ticks;
                }
            }
        };
        let apply = Arc::clone(&w).apply(fetched);
        tokio::pin!(apply);
        tokio::select! {
            _ = &mut apply => panic!("applied before the lock was free"),
            ticks = ticks => assert_eq!(ticks, 5),
        }
        release.send(()).unwrap();
        apply.await.unwrap();
        held.join().unwrap();
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
                false,
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

    #[tokio::test]
    async fn dropped_unconfirmed_tx_leaves_history() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
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
        let summary = w.sync(&chain).await.unwrap();
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

    /// A send of `sat` signed by the key of `account`, in the wallet.
    async fn signed_send(w: &SavingWallet, sat: u64, change_to: Option<ScriptBuf>) -> Transaction {
        let built = w
            .build_send(
                &Labels::default(),
                &SendPlan {
                    to: ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([9; 20])),
                    amount: Some(BtcAmount::from_sat(sat)),
                    fee_rate: FeeRate::from_sat_per_vb_u32(2),
                    change_to,
                    locked: HashSet::new(),
                    must_spend: Vec::new(),
                    only_must_spend: false,
                },
            )
            .unwrap();
        let mut psbt = built.psbt;
        SoftSigner::from_seed(&[7u8; 32], bitcoin::Network::Regtest)
            .sign(&mut psbt)
            .await
            .unwrap();
        let tx = w.finalize(psbt).unwrap();
        w.insert_sent(&tx).unwrap();
        tx
    }

    /// T3 M2: a sync must ask the server about what the wallet holds pending,
    /// or a real server's silence would never become an eviction.
    #[tokio::test]
    async fn a_sync_expects_what_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        fund(&w, 0, 10_000, Some(1));
        let (pending, _) = fund(&w, 1, 20_000, None);
        let chain = FakeChain::new();
        w.sync(&chain).await.unwrap();
        assert!(chain.expected().contains(&pending.txid));
    }

    /// T3 M2, through the request: nothing here injects an eviction.
    #[tokio::test]
    async fn a_dropped_send_returns_its_coin() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        let (coin, _) = fund(&w, 0, 100_000, Some(1));
        let tx = signed_send(&w, 30_000, None).await;
        let labels = Labels::default();
        assert!(!w.has_coin(&coin.to_string()), "the spend hides the coin");
        assert_eq!(w.tx_position(tx.compute_txid()), TxPosition::Pending);
        assert_eq!(w.history(&labels).len(), 2);

        let chain = FakeChain::new();
        chain.lose(tx.compute_txid());
        assert!(w.sync(&chain).await.unwrap().changed);

        assert!(w.has_coin(&coin.to_string()), "the coin came back");
        assert_eq!(w.balance(), Amount::from_sat(100_000));
        assert_eq!(w.history(&labels).len(), 1);
        assert_eq!(w.tx_position(tx.compute_txid()), TxPosition::Absent);
    }

    /// An eviction in the same second must not outrank the transaction's
    /// return: BDK treats equal times as evicted.
    #[tokio::test]
    async fn a_dropped_send_can_come_back_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        fund(&w, 0, 100_000, Some(1));
        let tx = signed_send(&w, 30_000, None).await;
        let chain = FakeChain::new();
        chain.lose(tx.compute_txid());
        w.sync(&chain).await.unwrap();
        w.insert_sent(&tx).unwrap();
        assert_eq!(w.tx_position(tx.compute_txid()), TxPosition::Pending);
    }

    #[tokio::test]
    async fn a_send_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        fund(&w, 0, 100_000, Some(1));
        let tx = signed_send(&w, 30_000, None).await;
        drop(w);
        let w = SavingWallet::open(&db(&dir), Network::Regtest)
            .unwrap()
            .unwrap();
        assert_eq!(w.tx_position(tx.compute_txid()), TxPosition::Pending);
    }

    #[tokio::test]
    async fn small_change_goes_to_the_internal_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        fund(&w, 0, 100_000, Some(1));
        let tx = signed_send(&w, 95_000, None).await;
        assert_eq!(tx.output.len(), 2);
        let change = tx
            .output
            .iter()
            .find(|o| o.value.to_sat() < 95_000)
            .unwrap();
        let inner = w.lock();
        let derived = inner.wallet.derivation_of_spk(change.script_pubkey.clone());
        assert_eq!(derived.map(|(k, _)| k), Some(KeychainKind::Internal));
        drop(inner);
        // So it counts as your own, not as money on its way from someone else.
        assert_eq!(w.in_flight(), Amount::ZERO);
        assert!(w.balance() > Amount::ZERO);
    }

    #[tokio::test]
    async fn drain_to_puts_the_change_on_that_script() {
        let dir = tempfile::tempdir().unwrap();
        let w = new_wallet(&dir);
        fund(&w, 0, 100_000, Some(1));
        let federation = ScriptBuf::new_p2wsh(&bitcoin::WScriptHash::from_byte_array([8; 32]));
        let tx = signed_send(&w, 30_000, Some(federation.clone())).await;
        assert!(tx.output.iter().any(|o| o.script_pubkey == federation));
        assert!(
            tx.output
                .iter()
                .all(|o| !w.lock().wallet.is_mine(o.script_pubkey.clone())),
            "no output came back to the wallet"
        );
    }

    #[tokio::test]
    async fn first_sync_is_a_full_scan_then_routine() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        let chain = FakeChain::new();
        let tip = w.tip().insert(BlockId {
            height: 3,
            hash: BlockHash::from_byte_array([3; 32]),
        });
        chain.push_full_scan(bdk_wallet::chain::spk_client::FullScanResponse {
            chain_update: Some(tip),
            ..Default::default()
        });
        assert!(!w.first_scan_done());
        let first = w.sync(&chain).await.unwrap();
        assert!(first.full_scan);
        assert!(w.first_scan_done());
        let second = w.sync(&chain).await.unwrap();
        assert!(!second.full_scan);
        assert_eq!(chain.full_scans_made(), 1);
        assert_eq!(chain.scans_made(), 2);
    }

    #[tokio::test]
    async fn offline_sync_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let w = Arc::new(new_wallet(&dir));
        fund(&w, 0, 10_000, Some(1));
        let chain = FakeChain::new();
        chain.set_offline(true);
        let err = w.sync(&chain).await.unwrap_err();
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
        let reference_dir = tempfile::tempdir().unwrap();
        let reference = new_wallet(&reference_dir);
        let expected = (0..151)
            .map(|_| reference.next_receive_address().unwrap())
            .last();
        assert_eq!(again.next_receive_address().ok(), expected);
    }
}
