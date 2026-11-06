//! SQLite persistence: settings, federations, history, watches and cursors.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::anonymity::{DepositSource, SessionDeposit};
use crate::api::{
    Amount, ChainBackend, DeviceKind, HistoryItem, HistoryKind, HistoryStatus, Network,
    OperationKind, SentChange, Settings, Timestamp, WalletKind,
};
use crate::error::{FedimixError, Result};
use crate::saving::{DeviceAccount, ExitLabel, Label, Labels, SentRecord, Tag};
use crate::scheduler::Origin;

const SCHEMA_VERSION: i64 = 10;

const SCHEMA: &str = "
CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK (id = 1), json TEXT NOT NULL);
CREATE TABLE federations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    invite TEXT NOT NULL,
    online_at INTEGER,
    balance_msat INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 0,
    position INTEGER NOT NULL,
    joined_session INTEGER,
    has_lightning INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE history (
    wallet TEXT NOT NULL,
    federation_id TEXT NOT NULL,
    id TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (wallet, federation_id, id)
);
CREATE INDEX history_order ON history (wallet, federation_id, timestamp DESC, id DESC);
CREATE TABLE watches (
    id TEXT PRIMARY KEY,
    federation_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    finished INTEGER NOT NULL DEFAULT 0,
    operation TEXT
);
CREATE TABLE cursors (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// The Saving tables (schema 5). Only public data: the device's account xpub,
/// never a private key. The BDK tables beside them are BDK's own.
const SAVING_SCHEMA: &str = "
CREATE TABLE saving_device (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    xpub TEXT NOT NULL,
    path TEXT NOT NULL
);
CREATE TABLE saving_labels (
    outpoint TEXT PRIMARY KEY,
    tag TEXT,
    estimate INTEGER,
    source_federation TEXT,
    frozen INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE saving_sent (
    txid TEXT PRIMARY KEY,
    change_msat INTEGER,
    to_spending INTEGER NOT NULL DEFAULT 0
);
";

/// Signed transactions (schema 6), kept from before their broadcast until the
/// chain settles them. Their rowid order is load-bearing: of sends that share a
/// coin the newest written is the one offered and holding the coins. So the
/// table is never vacuumed and never rebuilt without copying rowid, as either
/// can renumber a table whose key is not an `INTEGER PRIMARY KEY`. `change_address` is the federation address a transaction
/// pays, recorded here so that the row and the record are one write.
const SIGNED_SCHEMA: &str = "
CREATE TABLE saving_signed (
    id TEXT PRIMARY KEY,
    txid TEXT NOT NULL,
    tx_hex TEXT NOT NULL,
    state TEXT NOT NULL,
    change_address TEXT,
    created_at INTEGER NOT NULL
);
";

/// Schema 7: the transaction that confirmed instead of a stored one, so that
/// row still has something to show once BDK's view no longer lists it.
const LOST_TO: &str = "ALTER TABLE saving_signed ADD COLUMN lost_to TEXT;";

/// Schema 8: what a stored transaction is, so a refill taken up after a restart
/// still reports as one and shows as one. Rows from before it are sends.
const SIGNED_KIND: &str = "ALTER TABLE saving_signed ADD COLUMN kind TEXT NOT NULL DEFAULT 'send';";

/// Schema 9: the federation a stored transaction pays, if any, so money on its
/// way is counted against that federation alone. Rows from before it read as
/// unknown.
const SIGNED_FEDERATION: &str = "ALTER TABLE saving_signed ADD COLUMN federation_id TEXT;";

/// The exit queue (schema 10). `exit_items` lists every item until it lands,
/// oldest first by `seq`. `exit_addresses` keeps each address an exit was
/// given, also after a failed exit, so a coin there is always labelled and the
/// address is never given again. `anon_deposits` holds the countable deposits
/// of each synced session.
const EXIT_SCHEMA: &str = "
CREATE TABLE exit_items (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    federation_id TEXT NOT NULL,
    denomination_msat INTEGER NOT NULL,
    origin_session INTEGER NOT NULL,
    origin_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    state TEXT NOT NULL,
    intent TEXT UNIQUE,
    address TEXT,
    txid TEXT,
    fee_msat INTEGER,
    estimate INTEGER,
    early INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE exit_addresses (
    address TEXT PRIMARY KEY,
    item_id TEXT NOT NULL,
    federation_id TEXT NOT NULL,
    denomination_msat INTEGER NOT NULL,
    estimate INTEGER NOT NULL,
    early INTEGER NOT NULL
);
CREATE TABLE anon_deposits (
    federation_id TEXT NOT NULL,
    session INTEGER NOT NULL,
    position INTEGER NOT NULL,
    source TEXT NOT NULL,
    amount_msat INTEGER NOT NULL,
    reference TEXT,
    PRIMARY KEY (federation_id, session, position)
);
";

/// A write transaction that takes the write lock up front, so it waits for the
/// busy timeout instead of failing halfway.
fn begin(conn: &Connection) -> Result<Transaction<'_>> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(db_err)
}

pub(crate) fn db_err(e: impl std::fmt::Display) -> FedimixError {
    FedimixError::Internal(format!(
        "Fedimix could not read or write its database: {e}."
    ))
}

/// SQLite stores signed 64-bit integers. Unix seconds and msat amounts fit;
/// cursors may use the whole u64 range, so these casts keep the bits.
fn to_db(v: u64) -> i64 {
    v as i64
}

fn from_db(v: i64) -> u64 {
    v as u64
}

fn wallet_name(w: WalletKind) -> &'static str {
    match w {
        WalletKind::Saving => "saving",
        WalletKind::Spending => "spending",
    }
}

/// Upgrades the database from `from` to `from + 1`.
fn migrate_step(tx: &Transaction<'_>, from: i64) -> Result<()> {
    match from {
        1 => tx
            .execute_batch("ALTER TABLE watches ADD COLUMN operation TEXT;")
            .map_err(db_err),
        2 => tx
            .execute_batch("ALTER TABLE federations ADD COLUMN joined_session INTEGER;")
            .map_err(db_err),
        // Rows from before this were never asked, and warning is the rare case.
        3 => tx
            .execute_batch(
                "ALTER TABLE federations ADD COLUMN has_lightning INTEGER NOT NULL DEFAULT 1;",
            )
            .map_err(db_err),
        4 => tx.execute_batch(SAVING_SCHEMA).map_err(db_err),
        5 => tx.execute_batch(SIGNED_SCHEMA).map_err(db_err),
        6 => tx.execute_batch(LOST_TO).map_err(db_err),
        7 => tx.execute_batch(SIGNED_KIND).map_err(db_err),
        8 => tx.execute_batch(SIGNED_FEDERATION).map_err(db_err),
        9 => tx.execute_batch(EXIT_SCHEMA).map_err(db_err),
        _ => Err(FedimixError::Internal(format!(
            "Fedimix cannot upgrade its database from version {from}."
        ))),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FederationRow {
    pub id: String,
    pub name: String,
    pub invite: String,
    pub online_at: Option<Timestamp>,
    /// Last known balance, shown while offline.
    pub balance_msat: u64,
    /// The federation's session count at the join, if it could be read.
    pub joined_session: Option<u64>,
    pub has_lightning: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    LnReceive,
    LnSend,
    Deposit,
    Withdraw,
    /// A withdrawal of a queued denomination to Saving. Resolved as a send,
    /// but no Spending send.
    Exit,
}

impl WatchKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::LnReceive => "ln_receive",
            Self::LnSend => "ln_send",
            Self::Deposit => "deposit",
            Self::Withdraw => "withdraw",
            Self::Exit => "exit",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "ln_receive" => Self::LnReceive,
            "ln_send" => Self::LnSend,
            "deposit" => Self::Deposit,
            "withdraw" => Self::Withdraw,
            "exit" => Self::Exit,
            other => return Err(db_err(format!("unknown watch kind {other:?}"))),
        })
    }
}

/// An operation to follow until it settles, kept across restarts. A send's id
/// is its intent, which exists before the payment does: the store keeps the
/// operation it became beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Watch {
    pub id: String,
    pub federation_id: String,
    pub kind: WatchKind,
    pub created_at: Timestamp,
}

/// Where a queued denomination is on its way to Saving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitState {
    Queued,
    /// Its withdrawal is written down and may have been submitted.
    Withdrawing,
    /// Its output has a confirmation in Saving.
    Landed,
}

impl ExitState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Withdrawing => "withdrawing",
            Self::Landed => "landed",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "queued" => Self::Queued,
            "withdrawing" => Self::Withdrawing,
            "landed" => Self::Landed,
            other => return Err(db_err(format!("unknown exit state {other:?}"))),
        })
    }
}

/// One denomination booked to leave Spending. The fields after `state`
/// describe the exit under way, and are `None` while it is queued.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExitItem {
    pub id: String,
    pub federation_id: String,
    pub denomination: Amount,
    pub origin: Origin,
    pub created_at: Timestamp,
    pub state: ExitState,
    pub intent: Option<String>,
    pub address: Option<String>,
    pub txid: Option<String>,
    pub fee: Option<Amount>,
    pub estimate: Option<u32>,
    pub early: bool,
}

/// What [`Store::begin_exit`] writes before a withdrawal is submitted. The
/// intent is `watch.id`; `row` is the Spending row it finishes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExitStart {
    pub item_id: String,
    pub address: String,
    pub estimate: u32,
    pub early: bool,
    pub fee: Amount,
    pub row: HistoryItem,
    pub watch: Watch,
}

const EXIT_COLUMNS: &str = "id, federation_id, denomination_msat, origin_session, origin_at,
    created_at, state, intent, address, txid, fee_msat, estimate, early";

fn exit_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ExitItem, String)> {
    Ok((
        ExitItem {
            id: r.get(0)?,
            federation_id: r.get(1)?,
            denomination: Amount {
                msat: from_db(r.get(2)?),
            },
            origin: Origin {
                session: from_db(r.get(3)?),
                at: from_db(r.get(4)?),
            },
            created_at: from_db(r.get(5)?),
            // Parsed by the caller, which can fail with a sentence.
            state: ExitState::Queued,
            intent: r.get(7)?,
            address: r.get(8)?,
            txid: r.get(9)?,
            fee: r.get::<_, Option<i64>>(10)?.map(|msat| Amount {
                msat: from_db(msat),
            }),
            estimate: r
                .get::<_, Option<i64>>(11)?
                .and_then(|e| u32::try_from(e).ok()),
            early: r.get(12)?,
        },
        r.get(6)?,
    ))
}

fn source_str(source: DepositSource) -> &'static str {
    match source {
        DepositSource::OnChain => "on_chain",
        DepositSource::Lightning => "lightning",
    }
}

fn source_parse(s: &str) -> Result<DepositSource> {
    match s {
        "on_chain" => Ok(DepositSource::OnChain),
        "lightning" => Ok(DepositSource::Lightning),
        other => Err(db_err(format!("unknown deposit source {other:?}"))),
    }
}

/// The next session to sync of `federation`.
fn session_cursor_name(federation: &str) -> String {
    format!("anon:{federation}")
}

/// How far a signed transaction has come. `Signed` is an operation still
/// running: no server is known to have it. `Seen` went out and may yet be lost,
/// so its coins stay signed away. `Stalled` is held by no server we can reach
/// and may still confirm: the next send must spend its coins. The last two are
/// final: `Confirmed` is 6 blocks deep, and `Dropped` cannot confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignedState {
    Signed,
    Seen,
    Stalled,
    Confirmed,
    Dropped,
}

impl SignedState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Signed => "signed",
            Self::Seen => "seen",
            Self::Stalled => "stalled",
            Self::Confirmed => "confirmed",
            Self::Dropped => "dropped",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "signed" => Self::Signed,
            "seen" => Self::Seen,
            "stalled" => Self::Stalled,
            "confirmed" => Self::Confirmed,
            "dropped" => Self::Dropped,
            other => return Err(db_err(format!("unknown signed state {other:?}"))),
        })
    }
}

/// A transaction the device signed, as stored before its broadcast.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SignedTx {
    pub id: String,
    pub txid: String,
    pub tx_hex: String,
    pub state: SignedState,
    pub change_address: Option<String>,
    pub created_at: Timestamp,
    /// The transaction that spent the same coins and confirmed instead.
    pub lost_to: Option<String>,
    /// `Send` or `Refill`.
    pub kind: OperationKind,
    /// The federation `change_address` belongs to; `None` when it pays none,
    /// or for a row from before schema 9.
    pub federation_id: Option<String>,
}

fn signed_kind_str(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Refill => "refill",
        // Only Saving payments are stored here.
        OperationKind::Send | OperationKind::Exit => "send",
    }
}

fn signed_kind_parse(s: &str) -> Result<OperationKind> {
    match s {
        "send" => Ok(OperationKind::Send),
        "refill" => Ok(OperationKind::Refill),
        other => Err(db_err(format!("unknown signed kind {other:?}"))),
    }
}

/// What [`Store::upsert_history`] did. Only the store sees old and new in one
/// transaction, so a caller that must act once on a change asks it, not the row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HistoryWrite {
    Added,
    Changed {
        was: HistoryStatus,
    },
    /// The row already says this, or holds a final status that a replayed
    /// earlier update must not take back. Nothing at all replaces `Complete`:
    /// that money arrived.
    Unchanged,
}

fn is_final(status: &HistoryStatus) -> bool {
    !matches!(
        status,
        HistoryStatus::Pending | HistoryStatus::Confirming { .. }
    )
}

fn upsert_in(
    tx: &Transaction<'_>,
    wallet: WalletKind,
    federation_id: &str,
    item: &HistoryItem,
) -> Result<HistoryWrite> {
    let wallet = wallet_name(wallet);
    let existing: Option<(i64, String)> = tx
        .query_row(
            "SELECT timestamp, json FROM history WHERE wallet = ?1 AND federation_id = ?2 AND id = ?3",
            params![wallet, federation_id, item.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(db_err)?;
    let mut item = item.clone();
    let mut outcome = HistoryWrite::Added;
    if let Some((timestamp, json)) = existing {
        item.timestamp = from_db(timestamp);
        // A row that no longer parses gives way to this one.
        outcome = match serde_json::from_str::<HistoryItem>(&json) {
            Ok(old) if old == item => return Ok(HistoryWrite::Unchanged),
            Ok(old)
                if (is_final(&old.status) && !is_final(&item.status))
                    || (old.status == HistoryStatus::Complete
                        && item.status != HistoryStatus::Complete) =>
            {
                return Ok(HistoryWrite::Unchanged);
            }
            Ok(old) => HistoryWrite::Changed { was: old.status },
            Err(_) => HistoryWrite::Changed {
                was: HistoryStatus::Pending,
            },
        };
    }
    let json = serde_json::to_string(&item).map_err(db_err)?;
    tx.execute(
        "INSERT INTO history (wallet, federation_id, id, timestamp, json)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(wallet, federation_id, id) DO UPDATE SET json = excluded.json",
        params![wallet, federation_id, item.id, to_db(item.timestamp), json],
    )
    .map_err(db_err)?;
    Ok(outcome)
}

fn insert_watch(conn: &Connection, watch: &Watch) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO watches (id, federation_id, kind, created_at, finished)
         VALUES (?1, ?2, ?3, ?4, 0)",
        params![
            watch.id,
            watch.federation_id,
            watch.kind.as_str(),
            to_db(watch.created_at)
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Synchronous; callers wrap it in a `Mutex` and keep calls short.
#[derive(Debug)]
pub(crate) struct Store {
    conn: Connection,
}

impl Store {
    /// Every commit is fsynced (`synchronous=FULL` in WAL mode), so a caller
    /// that wants a record to survive a crash must commit it before it acts
    /// (the history row or watch before the payment is sent).
    pub(crate) fn open(path: &Path) -> Result<Store> {
        // Create the file 0600 ourselves: SQLite would use the umask, and
        // it gives the -wal and -shm files the database's mode.
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| db_err(format!("{}: {e}", path.display())))?;
        let conn = Connection::open(path).map_err(db_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err)?;
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
            .map_err(db_err)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(FedimixError::Internal(format!(
                "Fedimix needs a file system that supports write-ahead logging, but this one gave journal mode {mode}."
            )));
        }
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(db_err)?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let tx = begin(&self.conn)?;
        let has_version: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version')",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if !has_version {
            tx.execute_batch(
                "CREATE TABLE schema_version (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    version INTEGER NOT NULL
                );",
            )
            .map_err(db_err)?;
            tx.execute_batch(SCHEMA).map_err(db_err)?;
            tx.execute_batch(SAVING_SCHEMA).map_err(db_err)?;
            tx.execute_batch(SIGNED_SCHEMA).map_err(db_err)?;
            tx.execute_batch(LOST_TO).map_err(db_err)?;
            tx.execute_batch(SIGNED_KIND).map_err(db_err)?;
            tx.execute_batch(SIGNED_FEDERATION).map_err(db_err)?;
            tx.execute_batch(EXIT_SCHEMA).map_err(db_err)?;
            tx.execute(
                "INSERT INTO schema_version (id, version) VALUES (1, ?1)",
                [SCHEMA_VERSION],
            )
            .map_err(db_err)?;
            return tx.commit().map_err(db_err);
        }
        let mut version: i64 = tx
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .optional()
            .map_err(db_err)?
            .ok_or_else(|| {
                FedimixError::Internal(
                    "The database has no schema version, so Fedimix cannot tell what it holds. Restore it from your backup."
                        .into(),
                )
            })?;
        if version > SCHEMA_VERSION {
            return Err(FedimixError::Internal(
                "This data folder was made by a newer version of Fedimix. Update Fedimix to open it."
                    .into(),
            ));
        }
        // All steps and the version bump commit together.
        while version < SCHEMA_VERSION {
            migrate_step(&tx, version)?;
            version += 1;
            tx.execute("UPDATE schema_version SET version = ?1", [version])
                .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    pub(crate) fn settings(&self) -> Result<Settings> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT json FROM settings WHERE id = 1", [], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        let network = self.network()?;
        let mut settings = match json {
            None => return Ok(Settings::for_network(network)),
            Some(json) => serde_json::from_str::<Settings>(&json).map_err(db_err)?,
        };
        // The mainnet default in another network's folder was never chosen
        // there: an older version wrote it. Read it as unset.
        if network != Network::Bitcoin
            && settings.chain_backend
                == (ChainBackend::Esplora {
                    url: Network::Bitcoin.default_esplora_url().to_string(),
                })
        {
            settings.chain_backend = Settings::for_network(network).chain_backend;
        }
        Ok(settings)
    }

    /// The network the folder is bound to; mainnet before it is bound.
    fn network(&self) -> Result<Network> {
        let name: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'network'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_err)?;
        Ok(name
            .and_then(|name| name.parse().ok())
            .unwrap_or(Network::Bitcoin))
    }

    pub(crate) fn set_settings(&self, settings: &Settings) -> Result<()> {
        let json = serde_json::to_string(settings).map_err(db_err)?;
        self.conn
            .execute(
                "INSERT INTO settings (id, json) VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json",
                [json],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// In the order they were added.
    pub(crate) fn federations(&self) -> Result<Vec<FederationRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, name, invite, online_at, balance_msat, joined_session, has_lightning
                 FROM federations ORDER BY position",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(FederationRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    invite: r.get(2)?,
                    online_at: r.get::<_, Option<i64>>(3)?.map(from_db),
                    balance_msat: from_db(r.get(4)?),
                    joined_session: r.get::<_, Option<i64>>(5)?.map(from_db),
                    has_lightning: r.get(6)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(db_err)
    }

    /// Adding a known id updates its row and keeps its place and active flag.
    /// The first federation becomes the active one, in the same write.
    pub(crate) fn add_federation(&self, row: &FederationRow) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO federations
                    (id, name, invite, online_at, balance_msat, joined_session, has_lightning,
                     position, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                    (SELECT COALESCE(MAX(position), 0) + 1 FROM federations),
                    NOT EXISTS (SELECT 1 FROM federations WHERE active = 1))
                 ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name, invite = excluded.invite,
                    online_at = excluded.online_at, balance_msat = excluded.balance_msat,
                    joined_session = COALESCE(joined_session, excluded.joined_session),
                    has_lightning = excluded.has_lightning",
                params![
                    row.id,
                    row.name,
                    row.invite,
                    row.online_at.map(to_db),
                    to_db(row.balance_msat),
                    row.joined_session.map(to_db),
                    row.has_lightning
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Returns whether the stored balance changed.
    pub(crate) fn set_federation_balance(&self, id: &str, balance_msat: u64) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE federations SET balance_msat = ?2 WHERE id = ?1 AND balance_msat != ?2",
                params![id, to_db(balance_msat)],
            )
            .map_err(db_err)?;
        Ok(changed > 0)
    }

    pub(crate) fn set_online_at(&self, id: &str, at: Timestamp) -> Result<()> {
        self.conn
            .execute(
                "UPDATE federations SET online_at = ?2 WHERE id = ?1",
                params![id, to_db(at)],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub(crate) fn active_federation(&self) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT id FROM federations WHERE active = 1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_err)
    }

    pub(crate) fn set_active(&self, id: &str) -> Result<()> {
        let tx = begin(&self.conn)?;
        tx.execute("UPDATE federations SET active = 0", [])
            .map_err(db_err)?;
        let n = tx
            .execute("UPDATE federations SET active = 1 WHERE id = ?1", [id])
            .map_err(db_err)?;
        if n == 0 {
            return Err(FedimixError::UnknownItem(id.to_string()));
        }
        tx.commit().map_err(db_err)
    }

    /// Saving has no federation: pass `None`. An id already stored keeps its
    /// place in the list: only its content changes, never its timestamp.
    pub(crate) fn upsert_history(
        &self,
        wallet: WalletKind,
        federation_id: Option<&str>,
        item: &HistoryItem,
    ) -> Result<HistoryWrite> {
        let tx = begin(&self.conn)?;
        let outcome = upsert_in(&tx, wallet, federation_id.unwrap_or(""), item)?;
        tx.commit().map_err(db_err)?;
        Ok(outcome)
    }

    pub(crate) fn history_item(
        &self,
        wallet: WalletKind,
        federation_id: Option<&str>,
        id: &str,
    ) -> Result<Option<HistoryItem>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM history WHERE wallet = ?1 AND federation_id = ?2 AND id = ?3",
                params![wallet_name(wallet), federation_id.unwrap_or(""), id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        json.map(|json| serde_json::from_str(&json).map_err(db_err))
            .transpose()
    }

    /// Spending's deposit rows of `federation`, in no order. A row that no
    /// longer parses is skipped, as in [`Self::history_page`].
    pub(crate) fn deposit_rows(&self, federation_id: &str) -> Result<Vec<HistoryItem>> {
        self.spending_rows_of(federation_id, &HistoryKind::Deposit, "Deposit")
    }

    /// As [`Self::deposit_rows`], for Lightning receives.
    pub(crate) fn lightning_receive_rows(&self, federation_id: &str) -> Result<Vec<HistoryItem>> {
        self.spending_rows_of(
            federation_id,
            &HistoryKind::ReceivedLightning,
            "ReceivedLightning",
        )
    }

    /// Spending's rows of `kind`, whose JSON holds `name` quoted.
    fn spending_rows_of(
        &self,
        federation_id: &str,
        kind: &HistoryKind,
        name: &str,
    ) -> Result<Vec<HistoryItem>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT json FROM history WHERE wallet = ?1 AND federation_id = ?2
                 AND json LIKE ?3",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(
                params![
                    wallet_name(WalletKind::Spending),
                    federation_id,
                    format!("%\"{name}\"%")
                ],
                |r| r.get::<_, String>(0),
            )
            .map_err(db_err)?;
        let mut out = Vec::new();
        for json in rows {
            let json = json.map_err(db_err)?;
            match serde_json::from_str::<HistoryItem>(&json) {
                Ok(item) if item.kind == *kind => out.push(item),
                Ok(_) => {}
                Err(e) => eprintln!("fedimix: skipping a history row that does not parse: {e}"),
            }
        }
        Ok(out)
    }

    /// Newest first by `(timestamp, id)` within one wallet and federation
    /// (`None` for Saving). `before` is the id of the last item the caller
    /// has; rows that arrive later never shift the next page. A row that no
    /// longer parses is skipped with a warning rather than failing the page.
    pub(crate) fn history_page(
        &self,
        wallet: WalletKind,
        federation_id: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HistoryItem>> {
        let wallet = wallet_name(wallet);
        let federation_id = federation_id.unwrap_or("");
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let jsons: Vec<String> = match before {
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT json FROM history WHERE wallet = ?1 AND federation_id = ?2
                         ORDER BY timestamp DESC, id DESC LIMIT ?3",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![wallet, federation_id, limit], |r| r.get(0))
                    .map_err(db_err)?;
                rows.collect::<std::result::Result<_, _>>()
                    .map_err(db_err)?
            }
            Some(id) => {
                let ts: i64 = self
                    .conn
                    .query_row(
                        "SELECT timestamp FROM history
                         WHERE wallet = ?1 AND federation_id = ?2 AND id = ?3",
                        params![wallet, federation_id, id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(db_err)?
                    .ok_or_else(|| FedimixError::UnknownItem(id.to_string()))?;
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT json FROM history
                         WHERE wallet = ?1 AND federation_id = ?2 AND (timestamp, id) < (?3, ?4)
                         ORDER BY timestamp DESC, id DESC LIMIT ?5",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![wallet, federation_id, ts, id, limit], |r| r.get(0))
                    .map_err(db_err)?;
                rows.collect::<std::result::Result<_, _>>()
                    .map_err(db_err)?
            }
        };
        let mut out = Vec::with_capacity(jsons.len());
        for json in jsons {
            match serde_json::from_str(&json) {
                Ok(item) => out.push(item),
                Err(e) => eprintln!("fedimix: skipping a history row that cannot be read: {e}"),
            }
        }
        Ok(out)
    }

    /// Watches not yet finished, oldest first.
    pub(crate) fn watches(&self) -> Result<Vec<Watch>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, federation_id, kind, created_at FROM watches
                 WHERE finished = 0 ORDER BY created_at, id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?, r.get(3)?))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, federation_id, kind, created_at) = row.map_err(db_err)?;
            out.push(Watch {
                id,
                federation_id,
                kind: WatchKind::parse(&kind)?,
                created_at: from_db(created_at),
            });
        }
        Ok(out)
    }

    /// A Spending row for a deposit still on its way, keyed on its watch, which
    /// it takes back if the cap had dropped it: that watch's task runs until the
    /// connection ends, and a row needs its watch to ever reach a final status.
    pub(crate) fn upsert_progress(
        &self,
        federation_id: &str,
        item: &HistoryItem,
    ) -> Result<HistoryWrite> {
        let tx = begin(&self.conn)?;
        let outcome = upsert_in(&tx, WalletKind::Spending, federation_id, item)?;
        if outcome != HistoryWrite::Unchanged {
            tx.execute("UPDATE watches SET finished = 0 WHERE id = ?1", [&item.id])
                .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(outcome)
    }

    /// Whether the seed was created together with this store. The first answer
    /// stands: `seed_is_new` means nothing on later opens.
    pub(crate) fn seed_born_with_store(&self, seed_is_new: bool) -> Result<bool> {
        let tx = begin(&self.conn)?;
        tx.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('seed_born_with_store', ?1)",
            [if seed_is_new { "true" } else { "false" }],
        )
        .map_err(db_err)?;
        let value: String = tx
            .query_row(
                "SELECT value FROM meta WHERE key = 'seed_born_with_store'",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(value == "true")
    }

    /// Whether a wallet has lived here: a federation, or a seed once recorded.
    pub(crate) fn holds_a_wallet(&self) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM federations)
                     OR EXISTS (SELECT 1 FROM meta WHERE key = 'seed_born_with_store')",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)
    }

    #[cfg(test)]
    pub(crate) fn add_watch(&self, watch: &Watch) -> Result<()> {
        insert_watch(&self.conn, watch)
    }

    /// A Spending row and the watch that will finish it, in one write: a crash
    /// must not leave a row nothing will ever update.
    pub(crate) fn add_tracked(&self, item: &HistoryItem, watch: &Watch) -> Result<()> {
        let tx = begin(&self.conn)?;
        upsert_in(&tx, WalletKind::Spending, &watch.federation_id, item)?;
        insert_watch(&tx, watch)?;
        tx.commit().map_err(db_err)
    }

    /// Adds a deposit watch, then keeps at most `cap` unused ones of its
    /// federation by finishing the oldest. Unused means no deposit has shown up
    /// yet: a watch with a history row has money on its way.
    pub(crate) fn add_deposit_watch(&self, watch: &Watch, cap: usize) -> Result<()> {
        let tx = begin(&self.conn)?;
        insert_watch(&tx, watch)?;
        tx.execute(
            "UPDATE watches SET finished = 1 WHERE id IN (
                SELECT w.id FROM watches w
                WHERE w.federation_id = ?1 AND w.kind = ?2 AND w.finished = 0
                  AND NOT EXISTS (
                    SELECT 1 FROM history h
                    WHERE h.federation_id = w.federation_id AND h.id = w.id
                  )
                ORDER BY w.created_at DESC, w.rowid DESC
                LIMIT -1 OFFSET ?3
            )",
            params![
                watch.federation_id,
                WatchKind::Deposit.as_str(),
                i64::try_from(cap).unwrap_or(i64::MAX)
            ],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }

    /// Finishing an unknown id does nothing.
    pub(crate) fn finish_watch(&self, id: &str) -> Result<()> {
        self.conn
            .execute("UPDATE watches SET finished = 1 WHERE id = ?1", [id])
            .map_err(db_err)?;
        Ok(())
    }

    /// Sends and exits not yet finished, oldest first, each with its operation
    /// once known: an exit's intent is resolved as a send's is.
    pub(crate) fn sends(&self) -> Result<Vec<(Watch, Option<String>)>> {
        let mut out = Vec::new();
        for watch in self.watches()? {
            if matches!(
                watch.kind,
                WatchKind::LnSend | WatchKind::Withdraw | WatchKind::Exit
            ) {
                let operation = self.operation_of(&watch.id)?;
                out.push((watch, operation));
            }
        }
        Ok(out)
    }

    /// What intent `id` became, if that is known yet.
    pub(crate) fn operation_of(&self, id: &str) -> Result<Option<String>> {
        let operation = self
            .conn
            .query_row("SELECT operation FROM watches WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_err)?;
        Ok(operation.flatten())
    }

    pub(crate) fn set_operation(&self, id: &str, operation: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE watches SET operation = ?2 WHERE id = ?1",
                [id, operation],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Whether `id` is a send, running or finished.
    pub(crate) fn is_send(&self, id: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM watches WHERE id = ?1 AND kind IN (?2, ?3))",
                [id, WatchKind::LnSend.as_str(), WatchKind::Withdraw.as_str()],
                |r| r.get(0),
            )
            .map_err(db_err)
    }

    /// Ties the data folder to `network` on first use. Later uses must name
    /// the same one: federations of two networks must not share a folder.
    pub(crate) fn bind_network(&self, network: Network) -> Result<()> {
        let wanted = network.name();
        let tx = begin(&self.conn)?;
        tx.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('network', ?1)",
            [wanted],
        )
        .map_err(db_err)?;
        let folder: String = tx
            .query_row("SELECT value FROM meta WHERE key = 'network'", [], |r| {
                r.get(0)
            })
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        if folder != wanted {
            return Err(FedimixError::WrongNetwork {
                folder,
                wanted: wanted.to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn cursor(&self, name: &str) -> Result<Option<u64>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM cursors WHERE name = ?1", [name], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
            .map_err(db_err)?
            .map(from_db))
    }

    pub(crate) fn set_cursor(&self, name: &str, value: u64) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO cursors (name, value) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                params![name, to_db(value)],
            )
            .map_err(db_err)?;
        Ok(())
    }
}

/// The exit queue and the synced session deposits.
impl Store {
    fn exits_where(&self, condition: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<ExitItem>> {
        let sql = format!("SELECT {EXIT_COLUMNS} FROM exit_items WHERE {condition} ORDER BY seq");
        let mut stmt = self.conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt.query_map(args, exit_from_row).map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (mut item, state) = row.map_err(db_err)?;
            item.state = ExitState::parse(&state)?;
            out.push(item);
        }
        Ok(out)
    }

    /// The items not yet landed, of `federation` or of all, oldest first.
    pub(crate) fn exit_items(&self, federation: Option<&str>) -> Result<Vec<ExitItem>> {
        let landed = ExitState::Landed.as_str();
        match federation {
            Some(federation) => self.exits_where(
                "state != ?1 AND federation_id = ?2",
                &[&landed, &federation],
            ),
            None => self.exits_where("state != ?1", &[&landed]),
        }
    }

    /// Appends `items` in their order, all or none.
    pub(crate) fn add_exit_items(&self, items: &[ExitItem]) -> Result<()> {
        self.replace_queued_exits(&[], items)
    }

    /// Removes those of `ids` still queued and appends `items`, in one write.
    /// An item already withdrawing, or unknown, is left alone.
    pub(crate) fn replace_queued_exits(&self, ids: &[String], items: &[ExitItem]) -> Result<()> {
        let tx = begin(&self.conn)?;
        for id in ids {
            tx.execute(
                "DELETE FROM exit_items WHERE id = ?1 AND state = ?2",
                params![id, ExitState::Queued.as_str()],
            )
            .map_err(db_err)?;
        }
        for item in items {
            tx.execute(
                &format!(
                    "INSERT INTO exit_items ({EXIT_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
                ),
                params![
                    item.id,
                    item.federation_id,
                    to_db(item.denomination.msat),
                    to_db(item.origin.session),
                    to_db(item.origin.at),
                    to_db(item.created_at),
                    item.state.as_str(),
                    item.intent,
                    item.address,
                    item.txid,
                    item.fee.map(|fee| to_db(fee.msat)),
                    item.estimate,
                    item.early
                ],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    /// The commit point of an exit, before anything is submitted: the item
    /// withdrawing to its address, the address's label, the Spending row and
    /// the watch, in one write. Refused for an item not queued, or an address
    /// any exit was ever given.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "exits are made from Phase 4 Task 6 on")
    )]
    pub(crate) fn begin_exit(&self, start: &ExitStart) -> Result<()> {
        let tx = begin(&self.conn)?;
        let denomination: Option<(i64, String)> = tx
            .query_row(
                "SELECT denomination_msat, federation_id FROM exit_items
                 WHERE id = ?1 AND state = ?2",
                params![start.item_id, ExitState::Queued.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_err)?;
        let Some((denomination_msat, federation_id)) = denomination else {
            return Err(FedimixError::UnknownItem(start.item_id.clone()));
        };
        tx.execute(
            "UPDATE exit_items SET state = ?2, intent = ?3, address = ?4, txid = NULL,
                fee_msat = ?5, estimate = ?6, early = ?7
             WHERE id = ?1",
            params![
                start.item_id,
                ExitState::Withdrawing.as_str(),
                start.watch.id,
                start.address,
                to_db(start.fee.msat),
                start.estimate,
                start.early
            ],
        )
        .map_err(db_err)?;
        tx.execute(
            "INSERT INTO exit_addresses
                (address, item_id, federation_id, denomination_msat, estimate, early)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                start.address,
                start.item_id,
                federation_id,
                denomination_msat,
                start.estimate,
                start.early
            ],
        )
        .map_err(db_err)?;
        upsert_in(
            &tx,
            WalletKind::Spending,
            &start.watch.federation_id,
            &start.row,
        )?;
        insert_watch(&tx, &start.watch)?;
        tx.commit().map_err(db_err)
    }

    /// The item whose exit has the intent `intent`, while it has one.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "exits are made from Phase 4 Task 6 on")
    )]
    pub(crate) fn exit_by_intent(&self, intent: &str) -> Result<Option<ExitItem>> {
        Ok(self
            .exits_where("intent = ?1", &[&intent])?
            .into_iter()
            .next())
    }

    /// The federation signed the withdrawal as `txid`.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "exits are made from Phase 4 Task 6 on")
    )]
    pub(crate) fn exit_settled(&self, item_id: &str, txid: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE exit_items SET txid = ?2 WHERE id = ?1",
                [item_id, txid],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// A failed exit goes back to the queue with its origin. Its address
    /// keeps its label in `exit_addresses`. Only a withdrawing item moves.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "exits are made from Phase 4 Task 6 on")
    )]
    pub(crate) fn requeue_exit(&self, item_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE exit_items SET state = ?2, intent = NULL, address = NULL, txid = NULL
                 WHERE id = ?1 AND state = ?3",
                [
                    item_id,
                    ExitState::Queued.as_str(),
                    ExitState::Withdrawing.as_str(),
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "exits are made from Phase 4 Task 6 on")
    )]
    pub(crate) fn exit_landed(&self, item_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE exit_items SET state = ?2 WHERE id = ?1",
                [item_id, ExitState::Landed.as_str()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// The countable deposits of one session, and the cursor past it, in one
    /// write. A session written again replaces what it held.
    pub(crate) fn add_session(
        &self,
        federation: &str,
        session: u64,
        deposits: &[SessionDeposit],
    ) -> Result<()> {
        let tx = begin(&self.conn)?;
        tx.execute(
            "DELETE FROM anon_deposits WHERE federation_id = ?1 AND session = ?2",
            params![federation, to_db(session)],
        )
        .map_err(db_err)?;
        for (position, deposit) in deposits.iter().enumerate() {
            tx.execute(
                "INSERT INTO anon_deposits
                    (federation_id, session, position, source, amount_msat, reference)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    federation,
                    to_db(session),
                    i64::try_from(position).unwrap_or(i64::MAX),
                    source_str(deposit.source),
                    to_db(deposit.amount.msat),
                    deposit.reference
                ],
            )
            .map_err(db_err)?;
        }
        tx.execute(
            "INSERT INTO cursors (name, value) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            params![
                session_cursor_name(federation),
                to_db(session.saturating_add(1))
            ],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }

    /// The deposits of `federation` in sessions strictly after `after`, by
    /// session and then in session order.
    pub(crate) fn session_deposits(
        &self,
        federation: &str,
        after: u64,
    ) -> Result<Vec<(u64, SessionDeposit)>> {
        match after.checked_add(1) {
            Some(first) => self.session_deposits_from(federation, first),
            None => Ok(Vec::new()),
        }
    }

    /// As [`Self::session_deposits`], from session `first` on, which can be 0.
    pub(crate) fn session_deposits_from(
        &self,
        federation: &str,
        first: u64,
    ) -> Result<Vec<(u64, SessionDeposit)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session, source, amount_msat, reference FROM anon_deposits
                 WHERE federation_id = ?1 AND session >= ?2
                 ORDER BY session, position",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![federation, to_db(first)], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (session, source, msat, reference) = row.map_err(db_err)?;
            out.push((
                from_db(session),
                SessionDeposit {
                    source: source_parse(&source)?,
                    amount: Amount {
                        msat: from_db(msat),
                    },
                    reference,
                },
            ));
        }
        Ok(out)
    }

    /// The next session to sync of `federation`, once one was.
    pub(crate) fn session_cursor(&self, federation: &str) -> Result<Option<u64>> {
        self.cursor(&session_cursor_name(federation))
    }
}

/// Saving: the device account and the privacy labels.
impl Store {
    pub(crate) fn device_account(&self) -> Result<Option<DeviceAccount>> {
        let row = self
            .conn
            .query_row(
                "SELECT kind, name, fingerprint, xpub, path FROM saving_device WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(db_err)?;
        row.map(|(kind, name, fingerprint, xpub, path)| {
            Ok(DeviceAccount {
                kind: device_kind_parse(&kind)?,
                name,
                fingerprint,
                xpub,
                path,
            })
        })
        .transpose()
    }

    /// `false` when an account is already stored; nothing is changed then.
    pub(crate) fn set_device_account(&self, account: &DeviceAccount) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO saving_device (id, kind, name, fingerprint, xpub, path)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5)",
                params![
                    device_kind_str(account.kind),
                    account.name,
                    account.fingerprint,
                    account.xpub,
                    account.path
                ],
            )
            .map_err(db_err)?;
        Ok(n == 1)
    }

    pub(crate) fn saving_labels(&self) -> Result<Labels> {
        let mut coins = HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT outpoint, tag, estimate, source_federation, frozen FROM saving_labels")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })
            .map_err(db_err)?;
        for row in rows {
            let (outpoint, tag, estimate, source_federation, frozen) = row.map_err(db_err)?;
            let tag = tag.and_then(|name| {
                Tag::from_parts(&name, estimate.and_then(|e| u32::try_from(e).ok()))
            });
            coins.insert(
                outpoint,
                Label {
                    tag,
                    source_federation,
                    frozen,
                },
            );
        }
        let mut sent = HashMap::new();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT txid, change_msat, to_spending, EXISTS (
                    SELECT 1 FROM saving_signed
                    WHERE saving_signed.txid = saving_sent.txid AND kind = 'refill'
                 ) FROM saving_sent",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            })
            .map_err(db_err)?;
        for row in rows {
            let (txid, change, to_spending, refill) = row.map_err(db_err)?;
            let change = change.map(|msat| SentChange {
                amount: Amount {
                    msat: from_db(msat),
                },
                to_spending,
            });
            sent.insert(txid, SentRecord { change, refill });
        }
        let mut exits = HashMap::new();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT address, item_id, federation_id, denomination_msat, estimate, early
                 FROM exit_addresses",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    ExitLabel {
                        item_id: r.get(1)?,
                        federation_id: r.get(2)?,
                        denomination: Amount {
                            msat: from_db(r.get(3)?),
                        },
                        estimate: u32::try_from(r.get::<_, i64>(4)?).unwrap_or(u32::MAX),
                        early: r.get(5)?,
                    },
                ))
            })
            .map_err(db_err)?;
        for row in rows {
            let (address, label) = row.map_err(db_err)?;
            exits.insert(address, label);
        }
        Ok(Labels::from_rows(coins, sent, exits))
    }

    /// Sets the tag and source of `outpoint`, and leaves `frozen` alone.
    pub(crate) fn set_saving_tag(
        &self,
        outpoint: &str,
        tag: Tag,
        source_federation: Option<&str>,
    ) -> Result<()> {
        let (tag, estimate) = tag.to_parts();
        self.conn
            .execute(
                "INSERT INTO saving_labels (outpoint, tag, estimate, source_federation, frozen)
                 VALUES (?1, ?2, ?3, ?4, 0)
                 ON CONFLICT(outpoint) DO UPDATE SET tag = excluded.tag,
                    estimate = excluded.estimate,
                    source_federation = excluded.source_federation",
                params![outpoint, tag, estimate, source_federation],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Sets `frozen` of `outpoint`, and leaves its tag and source alone.
    pub(crate) fn set_saving_frozen(&self, outpoint: &str, frozen: bool) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO saving_labels (outpoint, frozen) VALUES (?1, ?2)
                 ON CONFLICT(outpoint) DO UPDATE SET frozen = excluded.frozen",
                params![outpoint, frozen],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// The commit point of a send: from here on it is rebroadcast until settled.
    pub(crate) fn add_signed(&self, signed: &SignedTx) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO saving_signed
                    (id, txid, tx_hex, state, change_address, created_at, kind, federation_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    signed.id,
                    signed.txid,
                    signed.tx_hex,
                    signed.state.as_str(),
                    signed.change_address,
                    to_db(signed.created_at),
                    signed_kind_str(signed.kind),
                    signed.federation_id
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn signed_where(&self, condition: &str, args: &[&str]) -> Result<Vec<SignedTx>> {
        let sql = format!(
            "SELECT id, txid, tx_hex, state, change_address, created_at, lost_to, kind,
                    federation_id
             FROM saving_signed WHERE {condition} ORDER BY rowid"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, txid, tx_hex, state, change_address, created_at, lost_to, kind, federation_id) =
                row.map_err(db_err)?;
            out.push(SignedTx {
                id,
                txid,
                tx_hex,
                state: SignedState::parse(&state)?,
                change_address,
                created_at: from_db(created_at),
                lost_to,
                kind: signed_kind_parse(&kind)?,
                federation_id,
            });
        }
        Ok(out)
    }

    /// The stored transaction `txid`, if any, and whether a row was written
    /// after `id`'s.
    pub(crate) fn signed_after(&self, txid: &str, id: &str) -> Result<Option<(SignedTx, bool)>> {
        let found = self.signed_where("txid = ?1", &[txid])?.into_iter().next();
        let Some(found) = found else {
            return Ok(None);
        };
        let later: bool = self
            .conn
            .query_row(
                "SELECT (SELECT rowid FROM saving_signed WHERE id = ?1)
                      > (SELECT rowid FROM saving_signed WHERE id = ?2)",
                [&found.id, id],
                |r| r.get::<_, Option<bool>>(0),
            )
            .map_err(db_err)?
            .unwrap_or(false);
        Ok(Some((found, later)))
    }

    /// The transactions not yet settled, oldest first: in the order written,
    /// which a clock in whole seconds cannot give.
    pub(crate) fn open_signed(&self) -> Result<Vec<SignedTx>> {
        self.signed_where(
            "state IN (?1, ?2, ?3)",
            &[
                SignedState::Signed.as_str(),
                SignedState::Seen.as_str(),
                SignedState::Stalled.as_str(),
            ],
        )
    }

    /// What Saving's history needs beside the wallet's view: the open
    /// transactions, and those another confirmed instead.
    pub(crate) fn signed_for_history(&self) -> Result<Vec<SignedTx>> {
        self.signed_where(
            "state IN (?1, ?2, ?3) OR lost_to IS NOT NULL",
            &[
                SignedState::Signed.as_str(),
                SignedState::Seen.as_str(),
                SignedState::Stalled.as_str(),
            ],
        )
    }

    /// The transactions 6 deep that pay a federation: their money is on its
    /// way until the federation credits it.
    pub(crate) fn confirmed_paying_federations(&self) -> Result<Vec<SignedTx>> {
        self.signed_where(
            "state = ?1 AND change_address IS NOT NULL",
            &[SignedState::Confirmed.as_str()],
        )
    }

    /// The transactions that pay `address`, but for those dropped.
    pub(crate) fn signed_paying(&self, address: &str) -> Result<Vec<SignedTx>> {
        self.signed_where(
            "change_address = ?1 AND state != ?2",
            &[address, SignedState::Dropped.as_str()],
        )
    }

    pub(crate) fn set_lost_to(&self, id: &str, lost_to: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE saving_signed SET lost_to = ?2 WHERE id = ?1",
                params![id, lost_to],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub(crate) fn set_signed_state(&self, id: &str, state: SignedState) -> Result<()> {
        self.conn
            .execute(
                "UPDATE saving_signed SET state = ?2 WHERE id = ?1",
                [id, state.as_str()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// What the stored transaction `id` is, if there is one.
    pub(crate) fn signed_kind(&self, id: &str) -> Result<Option<OperationKind>> {
        let row = self.signed_where("id = ?1", &[id])?.into_iter().next();
        Ok(row.map(|row| row.kind))
    }

    /// Whether `id` is a stored transaction.
    pub(crate) fn is_signed(&self, id: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM saving_signed WHERE id = ?1)",
                [id],
                |r| r.get(0),
            )
            .map_err(db_err)
    }

    /// Whether `txid` is a stored transaction's: a payment from Saving.
    pub(crate) fn is_signed_txid(&self, txid: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM saving_signed WHERE txid = ?1)",
                [txid],
                |r| r.get(0),
            )
            .map_err(db_err)
    }

    pub(crate) fn put_saving_sent(&self, txid: &str, sent: &SentRecord) -> Result<()> {
        let (msat, to_spending) = match &sent.change {
            Some(c) => (Some(to_db(c.amount.msat)), c.to_spending),
            None => (None, false),
        };
        let tx = begin(&self.conn)?;
        tx.execute(
            "INSERT INTO saving_sent (txid, change_msat, to_spending) VALUES (?1, ?2, ?3)
             ON CONFLICT(txid) DO UPDATE SET change_msat = excluded.change_msat,
                to_spending = excluded.to_spending",
            params![txid, msat, to_spending],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }
}

fn device_kind_str(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Ledger => "ledger",
        DeviceKind::Trezor => "trezor",
        DeviceKind::Coldcard => "coldcard",
        DeviceKind::Jade => "jade",
        DeviceKind::BitBox => "bitbox",
        DeviceKind::Software => "software",
    }
}

fn device_kind_parse(s: &str) -> Result<DeviceKind> {
    Ok(match s {
        "ledger" => DeviceKind::Ledger,
        "trezor" => DeviceKind::Trezor,
        "coldcard" => DeviceKind::Coldcard,
        "jade" => DeviceKind::Jade,
        "bitbox" => DeviceKind::BitBox,
        "software" => DeviceKind::Software,
        other => return Err(db_err(format!("unknown device kind {other:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anonymity::{DepositSource, SessionDeposit};
    use crate::api::*;
    use crate::saving::ExitLabel;
    use crate::scheduler::Origin;

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("fedimix.db")).unwrap();
        (dir, store)
    }

    fn item(id: &str, timestamp: u64) -> HistoryItem {
        HistoryItem {
            id: id.to_string(),
            kind: HistoryKind::Paid,
            amount: Amount::from_sat(10),
            incoming: false,
            status: HistoryStatus::Pending,
            timestamp,
            txid: None,
            fee: None,
            session: None,
        }
    }

    fn ids(items: &[HistoryItem]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn settings_roundtrip_defaults_when_empty() {
        let (_d, store) = open();
        assert_eq!(store.settings().unwrap(), Settings::default());
        let changed = Settings {
            anonymity_target: 7,
            socks5_proxy: Some("127.0.0.1:9050".into()),
            chain_backend: ChainBackend::Electrum {
                url: "ssl://example.org:50002".into(),
            },
            ..Settings::default()
        };
        store.set_settings(&changed).unwrap();
        assert_eq!(store.settings().unwrap(), changed);
    }

    #[test]
    fn mainnet_url_in_a_signet_folder_reads_as_unset() {
        let (_d, store) = open();
        store.bind_network(Network::Signet).unwrap();
        // What an older version stored: the mainnet default, never chosen.
        store.set_settings(&Settings::default()).unwrap();
        assert_eq!(
            store.settings().unwrap().chain_backend,
            Settings::for_network(Network::Signet).chain_backend
        );
        // A server the user picked stays.
        let chosen = Settings {
            chain_backend: ChainBackend::Esplora {
                url: "https://esplora.example/signet/api".into(),
            },
            ..Settings::default()
        };
        store.set_settings(&chosen).unwrap();
        assert_eq!(store.settings().unwrap(), chosen);
    }

    #[test]
    fn reopen_keeps_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .add_federation(&FederationRow {
                    id: "f1".into(),
                    name: "One".into(),
                    invite: "fed1abc".into(),
                    online_at: Some(5),
                    balance_msat: 2_100_000_000_000_000_000,
                    joined_session: None,
                    has_lightning: true,
                })
                .unwrap();
            store.set_active("f1").unwrap();
            store
                .upsert_history(WalletKind::Spending, Some("f1"), &item("a", 1))
                .unwrap();
            store
                .add_watch(&Watch {
                    id: "w1".into(),
                    federation_id: "f1".into(),
                    kind: WatchKind::LnReceive,
                    created_at: 9,
                })
                .unwrap();
            store.set_cursor("eventlog", u64::MAX).unwrap();
        }
        let store = Store::open(&path).unwrap();
        let feds = store.federations().unwrap();
        assert_eq!(feds.len(), 1);
        assert_eq!(feds[0].balance_msat, 2_100_000_000_000_000_000);
        assert_eq!(feds[0].online_at, Some(5));
        assert_eq!(store.active_federation().unwrap().as_deref(), Some("f1"));
        assert_eq!(
            ids(&store
                .history_page(WalletKind::Spending, Some("f1"), None, 10)
                .unwrap()),
            ["a"]
        );
        assert_eq!(store.watches().unwrap().len(), 1);
        assert_eq!(store.cursor("eventlog").unwrap(), Some(u64::MAX));
        assert_eq!(store.cursor("other").unwrap(), None);
    }

    #[test]
    fn history_pages_are_stable_when_rows_arrive() {
        let (_d, store) = open();
        for (id, ts) in [("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 4)] {
            store
                .upsert_history(WalletKind::Spending, Some("f"), &item(id, ts))
                .unwrap();
        }
        // Newest first by (timestamp, id): e, d, c, b, a.
        let first = store
            .history_page(WalletKind::Spending, Some("f"), None, 2)
            .unwrap();
        assert_eq!(ids(&first), ["e", "d"]);
        store
            .upsert_history(WalletKind::Spending, Some("f"), &item("new", 100))
            .unwrap();
        let second = store
            .history_page(WalletKind::Spending, Some("f"), Some("d"), 2)
            .unwrap();
        assert_eq!(ids(&second), ["c", "b"]);
        let third = store
            .history_page(WalletKind::Spending, Some("f"), Some("b"), 5)
            .unwrap();
        assert_eq!(ids(&third), ["a"]);
        assert!(
            store
                .history_page(WalletKind::Saving, None, None, 5)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.history_page(WalletKind::Spending, Some("f"), Some("nope"), 5),
            Err(crate::FedimixError::UnknownItem("nope".into()))
        );
    }

    #[test]
    fn upsert_updates_status_without_moving_the_row() {
        let (_d, store) = open();
        store
            .upsert_history(WalletKind::Spending, Some("f"), &item("a", 1))
            .unwrap();
        store
            .upsert_history(WalletKind::Spending, Some("f"), &item("b", 2))
            .unwrap();
        let mut done = item("a", 50);
        done.status = HistoryStatus::Complete;
        store
            .upsert_history(WalletKind::Spending, Some("f"), &done)
            .unwrap();
        let page = store
            .history_page(WalletKind::Spending, Some("f"), None, 10)
            .unwrap();
        assert_eq!(ids(&page), ["b", "a"]);
        assert_eq!(page[1].status, HistoryStatus::Complete);
        assert_eq!(page[1].timestamp, 1);
    }

    #[test]
    fn upsert_reports_what_it_did() {
        let (_d, store) = open();
        let write = |item: &HistoryItem| {
            store
                .upsert_history(WalletKind::Spending, Some("f"), item)
                .unwrap()
        };
        let pending = item("a", 1);
        assert_eq!(write(&pending), HistoryWrite::Added);
        assert_eq!(write(&pending), HistoryWrite::Unchanged);
        let done = HistoryItem {
            status: HistoryStatus::Complete,
            ..pending.clone()
        };
        assert_eq!(
            write(&done),
            HistoryWrite::Changed {
                was: HistoryStatus::Pending
            }
        );
        assert_eq!(write(&done), HistoryWrite::Unchanged);
    }

    #[test]
    fn a_final_status_is_not_taken_back_by_a_replay() {
        let (_d, store) = open();
        let mut row = item("a", 1);
        row.status = HistoryStatus::Complete;
        store
            .upsert_history(WalletKind::Spending, Some("f"), &row)
            .unwrap();
        row.status = HistoryStatus::Confirming {
            confirmations: 1,
            required: 6,
        };
        assert_eq!(
            store.upsert_history(WalletKind::Spending, Some("f"), &row),
            Ok(HistoryWrite::Unchanged)
        );
        let page = store
            .history_page(WalletKind::Spending, Some("f"), None, 10)
            .unwrap();
        assert_eq!(page[0].status, HistoryStatus::Complete);
    }

    #[test]
    fn complete_is_not_replaced_even_by_a_final_status() {
        let (_d, store) = open();
        let mut row = item("a", 1);
        row.status = HistoryStatus::Complete;
        store
            .upsert_history(WalletKind::Spending, Some("f"), &row)
            .unwrap();
        for status in [
            HistoryStatus::Failed,
            HistoryStatus::Expired,
            HistoryStatus::Refunded,
        ] {
            row.status = status;
            assert_eq!(
                store.upsert_history(WalletKind::Spending, Some("f"), &row),
                Ok(HistoryWrite::Unchanged)
            );
        }
        assert_eq!(
            store
                .history_item(WalletKind::Spending, Some("f"), "a")
                .unwrap()
                .unwrap()
                .status,
            HistoryStatus::Complete
        );
    }

    #[test]
    fn a_seen_deposit_takes_its_watch_back() {
        let (_d, store) = open();
        let watch = Watch {
            id: "a".into(),
            federation_id: "f".into(),
            kind: WatchKind::Deposit,
            created_at: 1,
        };
        store.add_watch(&watch).unwrap();
        store.finish_watch("a").unwrap();
        let mut row = item("a", 1);
        row.status = HistoryStatus::Confirming {
            confirmations: 1,
            required: 6,
        };
        store.upsert_progress("f", &row).unwrap();
        assert_eq!(store.watches().unwrap(), vec![watch]);
    }

    #[test]
    fn progress_replayed_on_a_final_row_leaves_the_watch_finished() {
        let (_d, store) = open();
        let watch = Watch {
            id: "a".into(),
            federation_id: "f".into(),
            kind: WatchKind::Deposit,
            created_at: 1,
        };
        store.add_watch(&watch).unwrap();
        store.finish_watch("a").unwrap();
        let mut row = item("a", 1);
        row.status = HistoryStatus::Complete;
        store
            .upsert_history(WalletKind::Spending, Some("f"), &row)
            .unwrap();
        row.status = HistoryStatus::Pending;
        assert_eq!(
            store.upsert_progress("f", &row),
            Ok(HistoryWrite::Unchanged)
        );
        assert!(store.watches().unwrap().is_empty());
    }

    #[test]
    fn whether_the_seed_is_older_than_the_store_is_kept_from_the_first_answer() {
        let (_d, store) = open();
        assert!(store.seed_born_with_store(true).unwrap());
        assert!(store.seed_born_with_store(false).unwrap());
        let (_d, store) = open();
        assert!(!store.seed_born_with_store(false).unwrap());
        assert!(!store.seed_born_with_store(true).unwrap());
    }

    #[test]
    fn a_wallet_shows_by_its_seed_record_or_a_federation() {
        let (_d, store) = open();
        assert!(!store.holds_a_wallet().unwrap());
        store.seed_born_with_store(true).unwrap();
        assert!(store.holds_a_wallet().unwrap());
        let (_d, store) = open();
        store.add_federation(&federation("f")).unwrap();
        assert!(store.holds_a_wallet().unwrap());
    }

    #[test]
    fn a_history_row_is_found_by_id_within_its_federation() {
        let (_d, store) = open();
        let row = item("a", 1);
        store
            .upsert_history(WalletKind::Spending, Some("f"), &row)
            .unwrap();
        assert_eq!(
            store.history_item(WalletKind::Spending, Some("f"), "a"),
            Ok(Some(row))
        );
        assert_eq!(
            store.history_item(WalletKind::Spending, Some("g"), "a"),
            Ok(None)
        );
    }

    #[test]
    fn a_receive_writes_its_row_and_watch_together() {
        let (_d, store) = open();
        let watch = Watch {
            id: "a".into(),
            federation_id: "f".into(),
            kind: WatchKind::LnReceive,
            created_at: 1,
        };
        store.add_tracked(&item("a", 1), &watch).unwrap();
        assert_eq!(store.watches().unwrap(), vec![watch.clone()]);
        assert!(
            store
                .history_item(WalletKind::Spending, Some("f"), "a")
                .unwrap()
                .is_some()
        );

        let raw = rusqlite::Connection::open(store.conn.path().unwrap()).unwrap();
        raw.execute_batch(
            "CREATE TRIGGER no_rows BEFORE INSERT ON watches
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
        let second = Watch {
            id: "b".into(),
            ..watch
        };
        assert!(store.add_tracked(&item("b", 2), &second).is_err());
        assert_eq!(
            store.history_item(WalletKind::Spending, Some("f"), "b"),
            Ok(None),
            "no row without its watch"
        );
    }

    #[test]
    fn a_send_keeps_its_operation_beside_its_intent() {
        let (_d, store) = open();
        let send = Watch {
            id: "intent".into(),
            federation_id: "f".into(),
            kind: WatchKind::LnSend,
            created_at: 1,
        };
        let receive = Watch {
            id: "receive".into(),
            kind: WatchKind::LnReceive,
            ..send.clone()
        };
        store.add_tracked(&item("intent", 1), &send).unwrap();
        store.add_tracked(&item("receive", 1), &receive).unwrap();
        assert_eq!(store.sends().unwrap(), vec![(send.clone(), None)]);
        store.set_operation("intent", "op").unwrap();
        assert_eq!(
            store.sends().unwrap(),
            vec![(send.clone(), Some("op".to_string()))]
        );
        assert_eq!(store.operation_of("intent"), Ok(Some("op".into())));
        assert_eq!(store.operation_of("nothing"), Ok(None));
        store.finish_watch("intent").unwrap();
        assert_eq!(store.sends().unwrap(), Vec::new());
        assert_eq!(store.is_send("intent"), Ok(true), "also once finished");
        assert_eq!(store.is_send("receive"), Ok(false));
        assert_eq!(store.is_send("nothing"), Ok(false));
    }

    fn exit_item(id: &str, federation: &str, sat: u64, session: u64) -> ExitItem {
        ExitItem {
            id: id.into(),
            federation_id: federation.into(),
            denomination: Amount::from_sat(sat),
            origin: Origin {
                session,
                at: session * 10,
            },
            created_at: 1_000 - session,
            state: ExitState::Queued,
            intent: None,
            address: None,
            txid: None,
            fee: None,
            estimate: None,
            early: false,
        }
    }

    fn exit_start(item_id: &str, intent: &str, address: &str) -> ExitStart {
        ExitStart {
            item_id: item_id.into(),
            address: address.into(),
            estimate: 12,
            early: true,
            fee: Amount::from_sat(300),
            row: HistoryItem {
                kind: HistoryKind::ToSaving {
                    denomination: Amount::from_sat(100_000),
                },
                ..item(intent, 5)
            },
            watch: Watch {
                id: intent.into(),
                federation_id: "f".into(),
                kind: WatchKind::Exit,
                created_at: 5,
            },
        }
    }

    fn exit_ids(items: &[ExitItem]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    /// Makes every later insert into `table` fail.
    fn refuse(store: &Store, table: &str) {
        let raw = rusqlite::Connection::open(store.conn.path().unwrap()).unwrap();
        raw.execute_batch(&format!(
            "CREATE TRIGGER refuse_{table} BEFORE INSERT ON {table}
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;"
        ))
        .unwrap();
    }

    #[test]
    fn a_version_9_database_gains_the_exit_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP TABLE exit_items;
                     DROP TABLE exit_addresses;
                     DROP TABLE anon_deposits;
                     UPDATE schema_version SET version = 9;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.exit_items(None).unwrap(), Vec::new());
        assert_eq!(store.session_cursor("f").unwrap(), None);
        assert_eq!(store.session_deposits("f", 0).unwrap(), Vec::new());
        assert!(store.saving_labels().unwrap().exit_at("address").is_none());
        store
            .add_exit_items(&[exit_item("a", "f", 100_000, 1)])
            .unwrap();
        assert_eq!(exit_ids(&store.exit_items(None).unwrap()), ["a"]);
    }

    /// Oldest first is write order, whatever the clock said.
    #[test]
    fn exit_items_keep_their_order_and_origin() {
        let (_d, store) = open();
        store
            .add_exit_items(&[
                exit_item("a", "f", 20_000_000, 3),
                exit_item("b", "g", 200_000, 7),
            ])
            .unwrap();
        store
            .add_exit_items(&[exit_item("c", "f", 100_000, 9)])
            .unwrap();
        assert_eq!(
            store.exit_items(None).unwrap(),
            vec![
                exit_item("a", "f", 20_000_000, 3),
                exit_item("b", "g", 200_000, 7),
                exit_item("c", "f", 100_000, 9),
            ]
        );
        assert_eq!(exit_ids(&store.exit_items(Some("f")).unwrap()), ["a", "c"]);
        assert!(store.exit_items(Some("h")).unwrap().is_empty());
        // A duplicate id is refused, and nothing of its batch is written.
        assert!(
            store
                .add_exit_items(&[exit_item("d", "f", 100_000, 1), exit_item("a", "f", 1, 1)])
                .is_err()
        );
        assert_eq!(exit_ids(&store.exit_items(None).unwrap()), ["a", "b", "c"]);
    }

    #[test]
    fn begin_exit_writes_item_address_row_and_watch_together() {
        let (_d, store) = open();
        store
            .add_exit_items(&[
                exit_item("a", "f", 100_000, 1),
                exit_item("b", "f", 100_000, 2),
            ])
            .unwrap();
        let start = exit_start("a", "intent-a", "address-a");
        store.begin_exit(&start).unwrap();
        let item = store.exit_by_intent("intent-a").unwrap().unwrap();
        assert_eq!(item.id, "a");
        assert_eq!(item.state, ExitState::Withdrawing);
        assert_eq!(item.address.as_deref(), Some("address-a"));
        assert_eq!(item.intent.as_deref(), Some("intent-a"));
        assert_eq!(item.estimate, Some(12));
        assert_eq!(item.fee, Some(Amount::from_sat(300)));
        assert!(item.early);
        assert_eq!(item.origin, exit_item("a", "f", 100_000, 1).origin);
        assert_eq!(
            store.saving_labels().unwrap().exit_at("address-a"),
            Some(&ExitLabel {
                item_id: "a".into(),
                federation_id: "f".into(),
                denomination: Amount::from_sat(100_000),
                estimate: 12,
                early: true,
            })
        );
        assert_eq!(
            store
                .history_item(WalletKind::Spending, Some("f"), "intent-a")
                .unwrap(),
            Some(start.row.clone())
        );
        assert_eq!(store.watches().unwrap(), vec![start.watch.clone()]);
        // Only a queued item begins.
        assert!(
            store
                .begin_exit(&exit_start("a", "intent-x", "address-x"))
                .is_err()
        );

        store.exit_settled("a", "txid-a").unwrap();
        let settled = store.exit_by_intent("intent-a").unwrap().unwrap();
        assert_eq!(settled.txid.as_deref(), Some("txid-a"));
        assert_eq!(settled.state, ExitState::Withdrawing);
        store.exit_landed("a").unwrap();
        assert_eq!(exit_ids(&store.exit_items(None).unwrap()), ["b"]);

        refuse(&store, "watches");
        assert!(
            store
                .begin_exit(&exit_start("b", "intent-b", "address-b"))
                .is_err()
        );
        let b = &store.exit_items(None).unwrap()[0];
        assert_eq!(b.state, ExitState::Queued);
        assert_eq!(b.address, None);
        assert_eq!(b.intent, None);
        assert!(
            store
                .saving_labels()
                .unwrap()
                .exit_at("address-b")
                .is_none()
        );
        assert_eq!(
            store.history_item(WalletKind::Spending, Some("f"), "intent-b"),
            Ok(None)
        );
        assert_eq!(store.exit_by_intent("intent-b"), Ok(None));
    }

    /// A failed exit keeps its origin, and its address stays labelled for a
    /// late payment there, never to be used again.
    #[test]
    fn requeue_keeps_the_address_label() {
        let (_d, store) = open();
        let queued = exit_item("a", "f", 100_000, 4);
        store.add_exit_items(std::slice::from_ref(&queued)).unwrap();
        store
            .begin_exit(&exit_start("a", "intent-a", "address-a"))
            .unwrap();
        store.exit_settled("a", "txid-a").unwrap();
        store.requeue_exit("a").unwrap();
        let item = &store.exit_items(None).unwrap()[0];
        assert_eq!(item.state, ExitState::Queued);
        assert_eq!(item.intent, None);
        assert_eq!(item.address, None);
        assert_eq!(item.txid, None);
        assert_eq!(item.origin, queued.origin);
        assert_eq!(store.exit_by_intent("intent-a"), Ok(None));
        let labels = store.saving_labels().unwrap();
        assert_eq!(labels.exit_at("address-a").unwrap().item_id, "a");
        // The address is never handed out again.
        assert!(
            store
                .begin_exit(&exit_start("a", "intent-b", "address-a"))
                .is_err()
        );
        store
            .begin_exit(&exit_start("a", "intent-b", "address-b"))
            .unwrap();
    }

    #[test]
    fn replacing_skips_a_withdrawing_item() {
        let (_d, store) = open();
        store
            .add_exit_items(&[
                exit_item("a", "f", 100_000, 1),
                exit_item("b", "f", 100_000, 2),
                exit_item("c", "f", 100_000, 3),
            ])
            .unwrap();
        store
            .begin_exit(&exit_start("a", "intent-a", "address-a"))
            .unwrap();
        store
            .replace_queued_exits(
                &["a".into(), "b".into(), "unknown".into()],
                &[exit_item("d", "f", 100_000, 2)],
            )
            .unwrap();
        let left = store.exit_items(None).unwrap();
        assert_eq!(exit_ids(&left), ["a", "c", "d"]);
        assert_eq!(left[0].state, ExitState::Withdrawing);
    }

    #[test]
    fn a_session_and_its_cursor_are_one_write() {
        let (_d, store) = open();
        let on_chain = SessionDeposit {
            source: DepositSource::OnChain,
            amount: Amount::from_sat(100_000),
            reference: Some("txid:1".into()),
        };
        let lightning = SessionDeposit {
            source: DepositSource::Lightning,
            amount: Amount { msat: 4_983_000 },
            reference: None,
        };
        store
            .add_session("f", 5, &[on_chain.clone(), lightning.clone()])
            .unwrap();
        store.add_session("f", 6, &[]).unwrap();
        store
            .add_session("g", 9, std::slice::from_ref(&on_chain))
            .unwrap();
        assert_eq!(store.session_cursor("f").unwrap(), Some(7));
        assert_eq!(store.session_cursor("g").unwrap(), Some(10));
        assert_eq!(store.cursor("anon:f").unwrap(), Some(7));
        assert_eq!(
            store.session_deposits("f", 4).unwrap(),
            vec![(5, on_chain.clone()), (5, lightning)]
        );
        assert_eq!(store.session_deposits("f", 5).unwrap(), Vec::new());

        refuse(&store, "cursors");
        assert!(
            store
                .add_session("f", 7, std::slice::from_ref(&on_chain))
                .is_err()
        );
        assert_eq!(store.session_cursor("f").unwrap(), Some(7));
        assert_eq!(store.session_deposits("f", 6).unwrap(), Vec::new());
    }

    #[test]
    fn exit_watches_count_as_sends_for_resolving() {
        let (_d, store) = open();
        store
            .add_exit_items(&[exit_item("a", "f", 100_000, 1)])
            .unwrap();
        let start = exit_start("a", "intent-a", "address-a");
        store.begin_exit(&start).unwrap();
        assert_eq!(store.sends().unwrap(), vec![(start.watch.clone(), None)]);
        store.set_operation("intent-a", "op").unwrap();
        assert_eq!(
            store.sends().unwrap(),
            vec![(start.watch, Some("op".to_string()))]
        );
        assert_eq!(
            store.is_send("intent-a"),
            Ok(false),
            "it is no Spending send"
        );
    }

    #[test]
    fn a_version_1_database_gains_the_operation_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        let watch = Watch {
            id: "w".into(),
            federation_id: "f".into(),
            kind: WatchKind::Deposit,
            created_at: 1,
        };
        {
            let store = Store::open(&path).unwrap();
            store.add_watch(&watch).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP TABLE saving_signed;
                     DROP TABLE saving_device;
                     DROP TABLE saving_labels;
                     DROP TABLE saving_sent;
                     ALTER TABLE watches DROP COLUMN operation;
                     ALTER TABLE federations DROP COLUMN joined_session;
                     ALTER TABLE federations DROP COLUMN has_lightning;
                     DROP TABLE exit_items;
                     DROP TABLE exit_addresses;
                     DROP TABLE anon_deposits;
                     UPDATE schema_version SET version = 1;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.watches().unwrap(), vec![watch]);
        store.set_operation("w", "op").unwrap();
        assert_eq!(store.operation_of("w"), Ok(Some("op".into())));
    }

    #[test]
    fn a_version_2_database_gains_the_joined_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        {
            let store = Store::open(&path).unwrap();
            store.add_federation(&federation("f")).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP TABLE saving_signed;
                     DROP TABLE saving_device;
                     DROP TABLE saving_labels;
                     DROP TABLE saving_sent;
                     ALTER TABLE federations DROP COLUMN joined_session;
                     ALTER TABLE federations DROP COLUMN has_lightning;
                     DROP TABLE exit_items;
                     DROP TABLE exit_addresses;
                     DROP TABLE anon_deposits;
                     UPDATE schema_version SET version = 2;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.federations().unwrap()[0].joined_session, None);
        assert!(store.federations().unwrap()[0].has_lightning);
    }

    #[test]
    fn a_version_4_database_gains_the_saving_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP TABLE saving_signed;
                     DROP TABLE saving_device;
                     DROP TABLE saving_labels;
                     DROP TABLE saving_sent;
                     DROP TABLE exit_items;
                     DROP TABLE exit_addresses;
                     DROP TABLE anon_deposits;
                     UPDATE schema_version SET version = 4;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.device_account(), Ok(None));
        assert!(store.saving_labels().is_ok());
    }

    #[test]
    fn a_version_5_database_gains_the_signed_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP TABLE saving_signed;
                     DROP TABLE exit_items;
                     DROP TABLE exit_addresses;
                     DROP TABLE anon_deposits;
                     UPDATE schema_version SET version = 5;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.open_signed().unwrap(), Vec::new());
    }

    #[test]
    fn signed_rows_keep_their_state_address_and_loss() {
        let (_dir, store) = open();
        let signed = SignedTx {
            id: "saving-1".into(),
            txid: "txid".into(),
            tx_hex: "00".into(),
            state: SignedState::Signed,
            change_address: Some("address".into()),
            created_at: 7,
            lost_to: None,
            kind: OperationKind::Refill,
            federation_id: Some("fed".into()),
        };
        store.add_signed(&signed).unwrap();
        assert_eq!(
            store.signed_kind("saving-1").unwrap(),
            Some(OperationKind::Refill)
        );
        assert_eq!(store.signed_kind("saving-2").unwrap(), None);
        assert_eq!(store.open_signed().unwrap(), [signed]);
        store
            .set_signed_state("saving-1", SignedState::Seen)
            .unwrap();
        assert_eq!(store.open_signed().unwrap().len(), 1);
        store
            .set_signed_state("saving-1", SignedState::Stalled)
            .unwrap();
        assert_eq!(
            store.open_signed().unwrap().len(),
            1,
            "it may still confirm"
        );
        assert!(store.is_signed("saving-1").unwrap());
        assert_eq!(store.signed_paying("address").unwrap().len(), 1);
        assert!(!store.is_signed("saving-2").unwrap());
        store
            .set_signed_state("saving-1", SignedState::Confirmed)
            .unwrap();
        assert_eq!(store.open_signed().unwrap(), Vec::new());
        store
            .set_signed_state("saving-1", SignedState::Dropped)
            .unwrap();
        assert_eq!(store.signed_paying("address").unwrap(), Vec::new());
        assert_eq!(store.signed_for_history().unwrap(), Vec::new());
        store.set_lost_to("saving-1", Some("winner")).unwrap();
        let shown = store.signed_for_history().unwrap();
        assert_eq!(shown[0].lost_to.as_deref(), Some("winner"));
    }

    /// Which send is newest decides which is offered and which holds coins, so
    /// it is write order, whatever the clock said.
    #[test]
    fn signed_rows_come_in_write_order() {
        let (_dir, store) = open();
        for (n, created_at) in [(1, 9), (2, 5), (3, 7)] {
            store
                .add_signed(&SignedTx {
                    id: format!("saving-{n}"),
                    txid: format!("txid-{n}"),
                    tx_hex: "00".into(),
                    state: SignedState::Seen,
                    change_address: None,
                    created_at,
                    lost_to: None,
                    kind: OperationKind::Send,
                    federation_id: None,
                })
                .unwrap();
        }
        let ids: Vec<_> = store
            .open_signed()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["saving-1", "saving-2", "saving-3"]);
    }

    #[test]
    fn a_version_6_database_gains_the_lost_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .execute_batch(
                    "ALTER TABLE saving_signed DROP COLUMN lost_to;
                     ALTER TABLE saving_signed DROP COLUMN kind;
                     ALTER TABLE saving_signed DROP COLUMN federation_id;
                     DROP TABLE exit_items;
                     DROP TABLE exit_addresses;
                     DROP TABLE anon_deposits;
                     UPDATE schema_version SET version = 6;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.signed_for_history().unwrap(), Vec::new());
    }

    /// A row from before schema 8 is a send.
    #[test]
    fn a_version_7_database_gains_the_kind_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .execute_batch(
                    "ALTER TABLE saving_signed DROP COLUMN kind;
                     ALTER TABLE saving_signed DROP COLUMN federation_id;
                     INSERT INTO saving_signed (id, txid, tx_hex, state, created_at)
                     VALUES ('saving-1', 'txid', '00', 'seen', 1);
                     DROP TABLE exit_items;
                     DROP TABLE exit_addresses;
                     DROP TABLE anon_deposits;
                     UPDATE schema_version SET version = 7;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.signed_kind("saving-1").unwrap(),
            Some(OperationKind::Send)
        );
    }

    /// A sent record knows it is a refill from the signed row of its
    /// transaction.
    /// A row from before schema 9 pays no known federation.
    #[test]
    fn a_version_8_database_gains_the_federation_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .execute_batch(
                    "ALTER TABLE saving_signed DROP COLUMN federation_id;
                     INSERT INTO saving_signed (id, txid, tx_hex, state, created_at, kind)
                     VALUES ('saving-1', 'txid', '00', 'seen', 1, 'refill');
                     DROP TABLE exit_items;
                     DROP TABLE exit_addresses;
                     DROP TABLE anon_deposits;
                     UPDATE schema_version SET version = 8;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let rows = store.open_signed().unwrap();
        assert_eq!(rows[0].federation_id, None);
        assert_eq!(rows[0].kind, OperationKind::Refill);
    }

    #[test]
    fn a_refill_record_reads_as_a_refill() {
        let (_dir, store) = open();
        for (n, kind) in [(1, OperationKind::Send), (2, OperationKind::Refill)] {
            store
                .put_saving_sent(
                    &format!("txid-{n}"),
                    &SentRecord {
                        change: None,
                        refill: false,
                    },
                )
                .unwrap();
            store
                .add_signed(&SignedTx {
                    id: format!("saving-{n}"),
                    txid: format!("txid-{n}"),
                    tx_hex: "00".into(),
                    state: SignedState::Signed,
                    change_address: None,
                    created_at: 1,
                    lost_to: None,
                    kind,
                    federation_id: None,
                })
                .unwrap();
        }
        let labels = store.saving_labels().unwrap();
        assert!(!labels.sent("txid-1").unwrap().refill);
        assert!(labels.sent("txid-2").unwrap().refill);
    }

    #[test]
    fn deposit_watches_drop_the_oldest_unused_past_the_cap() {
        let (_d, store) = open();
        let watch = |id: &str, federation: &str| Watch {
            id: id.into(),
            federation_id: federation.into(),
            kind: WatchKind::Deposit,
            created_at: 5,
        };
        let add = |id: &str, federation: &str| {
            store.add_deposit_watch(&watch(id, federation), 2).unwrap();
        };
        add("seen", "f");
        store
            .upsert_history(WalletKind::Spending, Some("f"), &item("seen", 1))
            .unwrap();
        add("b", "f");
        add("other", "g");
        add("c", "f");
        add("d", "f");
        let ids: Vec<String> = store.watches().unwrap().into_iter().map(|w| w.id).collect();
        // Equal times keep the order of adding: of "b", "c", "d", the two newest
        // stay. "seen" has a row, and "other" another federation's count.
        assert_eq!(ids, ["c", "d", "other", "seen"]);
    }

    #[test]
    fn watches_finish() {
        let (_d, store) = open();
        let w = Watch {
            id: "w".into(),
            federation_id: "f".into(),
            kind: WatchKind::Withdraw,
            created_at: 1,
        };
        store.add_watch(&w).unwrap();
        assert_eq!(store.watches().unwrap(), vec![w]);
        store.finish_watch("w").unwrap();
        assert!(store.watches().unwrap().is_empty());
    }

    fn federation(id: &str) -> FederationRow {
        FederationRow {
            id: id.into(),
            name: id.into(),
            invite: "fed1".into(),
            online_at: None,
            balance_msat: 0,
            joined_session: None,
            has_lightning: true,
        }
    }

    #[test]
    fn first_added_federation_is_active_and_stays_so() {
        let (_d, store) = open();
        assert_eq!(store.active_federation().unwrap(), None);
        store.add_federation(&federation("a")).unwrap();
        store.add_federation(&federation("b")).unwrap();
        store.add_federation(&federation("a")).unwrap();
        assert_eq!(store.active_federation().unwrap().as_deref(), Some("a"));
        store.set_active("b").unwrap();
        store.add_federation(&federation("a")).unwrap();
        assert_eq!(store.active_federation().unwrap().as_deref(), Some("b"));
    }

    #[test]
    fn balance_updates_report_a_change() {
        let (_d, store) = open();
        store.add_federation(&federation("a")).unwrap();
        assert!(store.set_federation_balance("a", 5).unwrap());
        assert!(!store.set_federation_balance("a", 5).unwrap());
        assert!(!store.set_federation_balance("nope", 5).unwrap());
        store.set_online_at("a", 9).unwrap();
        let rows = store.federations().unwrap();
        assert_eq!((rows[0].balance_msat, rows[0].online_at), (5, Some(9)));
    }

    #[test]
    fn a_store_binds_to_the_first_network() {
        let (_d, store) = open();
        store.bind_network(Network::Signet).unwrap();
        store.bind_network(Network::Signet).unwrap();
        assert_eq!(
            store.bind_network(Network::Bitcoin),
            Err(FedimixError::WrongNetwork {
                folder: "signet".into(),
                wanted: "bitcoin".into(),
            })
        );
    }

    #[test]
    fn set_active_unknown_is_refused() {
        let (_d, store) = open();
        assert_eq!(
            store.set_active("zzz"),
            Err(crate::FedimixError::UnknownItem("zzz".into()))
        );
    }

    #[test]
    fn history_is_per_federation() {
        let (_d, store) = open();
        store
            .upsert_history(WalletKind::Spending, Some("f1"), &item("a", 1))
            .unwrap();
        store
            .upsert_history(WalletKind::Spending, Some("f2"), &item("b", 2))
            .unwrap();
        store
            .upsert_history(WalletKind::Spending, Some("f2"), &item("a", 3))
            .unwrap();
        store
            .upsert_history(WalletKind::Saving, None, &item("s", 4))
            .unwrap();
        let f1 = store
            .history_page(WalletKind::Spending, Some("f1"), None, 10)
            .unwrap();
        assert_eq!(ids(&f1), ["a"]);
        assert_eq!(f1[0].timestamp, 1);
        let f2 = store
            .history_page(WalletKind::Spending, Some("f2"), None, 1)
            .unwrap();
        assert_eq!(ids(&f2), ["a"]);
        assert_eq!(f2[0].timestamp, 3);
        // The cursor is resolved inside its own federation.
        let next = store
            .history_page(WalletKind::Spending, Some("f2"), Some("a"), 10)
            .unwrap();
        assert_eq!(ids(&next), ["b"]);
        assert_eq!(
            ids(&store
                .history_page(WalletKind::Saving, None, None, 10)
                .unwrap()),
            ["s"]
        );
    }

    #[test]
    fn data_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("data");
        crate::seed::open_data_dir(&dir).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        let db = dir.join("fedimix.db");
        let store = Store::open(&db).unwrap();
        store.set_cursor("c", 1).unwrap();
        assert_eq!(mode(&db), 0o600);
        for suffix in ["-wal", "-shm"] {
            let p = dir.join(format!("fedimix.db{suffix}"));
            if p.exists() {
                assert_eq!(mode(&p), 0o600, "{p:?}");
            }
        }
    }

    #[test]
    fn old_settings_row_gains_new_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        drop(Store::open(&path).unwrap());
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute(
            "INSERT INTO settings (id, json) VALUES (1, '{\"anonymity_target\": 3}')",
            [],
        )
        .unwrap();
        drop(raw);
        let got = Store::open(&path).unwrap().settings().unwrap();
        assert_eq!(got.anonymity_target, 3);
        assert_eq!(got.fee_guard_percent, Settings::default().fee_guard_percent);
    }

    #[test]
    fn unreadable_history_row_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        let store = Store::open(&path).unwrap();
        store
            .upsert_history(WalletKind::Spending, Some("f"), &item("a", 1))
            .unwrap();
        store
            .upsert_history(WalletKind::Spending, Some("f"), &item("c", 3))
            .unwrap();
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute(
            "INSERT INTO history (wallet, federation_id, id, timestamp, json)
             VALUES ('spending', 'f', 'b', 2, '{\"kind\": \"FromTheFuture\"}')",
            [],
        )
        .unwrap();
        let page = store
            .history_page(WalletKind::Spending, Some("f"), None, 10)
            .unwrap();
        assert_eq!(ids(&page), ["c", "a"]);
    }

    #[test]
    fn empty_version_row_gets_a_sentence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        drop(Store::open(&path).unwrap());
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute("DELETE FROM schema_version", []).unwrap();
        drop(raw);
        match Store::open(&path).err().unwrap() {
            crate::FedimixError::Internal(msg) => {
                assert!(msg.contains("no schema version"), "{msg}")
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fedimix.db");
        drop(Store::open(&path).unwrap());
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute("UPDATE schema_version SET version = 999", [])
            .unwrap();
        drop(raw);
        let err = Store::open(&path).err().unwrap();
        match err {
            crate::FedimixError::Internal(msg) => {
                assert!(msg.contains("newer version of Fedimix"), "{msg}")
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
