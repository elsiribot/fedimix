//! SQLite persistence: settings, federations, history, watches and cursors.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::api::{HistoryItem, HistoryStatus, Network, Settings, Timestamp, WalletKind};
use crate::error::{FedimixError, Result};

const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK (id = 1), json TEXT NOT NULL);
CREATE TABLE federations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    invite TEXT NOT NULL,
    online_at INTEGER,
    balance_msat INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 0,
    position INTEGER NOT NULL
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

/// A write transaction that takes the write lock up front, so it waits for the
/// busy timeout instead of failing halfway.
fn begin(conn: &Connection) -> Result<Transaction<'_>> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(db_err)
}

fn db_err(e: impl std::fmt::Display) -> FedimixError {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    LnReceive,
    LnSend,
    Deposit,
    Withdraw,
}

impl WatchKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::LnReceive => "ln_receive",
            Self::LnSend => "ln_send",
            Self::Deposit => "deposit",
            Self::Withdraw => "withdraw",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "ln_receive" => Self::LnReceive,
            "ln_send" => Self::LnSend,
            "deposit" => Self::Deposit,
            "withdraw" => Self::Withdraw,
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
        match json {
            None => Ok(Settings::default()),
            Some(json) => serde_json::from_str(&json).map_err(db_err),
        }
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
                "SELECT id, name, invite, online_at, balance_msat FROM federations ORDER BY position",
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
                "INSERT INTO federations (id, name, invite, online_at, balance_msat, position, active)
                 VALUES (?1, ?2, ?3, ?4, ?5,
                    (SELECT COALESCE(MAX(position), 0) + 1 FROM federations),
                    NOT EXISTS (SELECT 1 FROM federations WHERE active = 1))
                 ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name, invite = excluded.invite,
                    online_at = excluded.online_at, balance_msat = excluded.balance_msat",
                params![
                    row.id,
                    row.name,
                    row.invite,
                    row.online_at.map(to_db),
                    to_db(row.balance_msat)
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

    /// Sends not yet finished, oldest first, each with its operation once known.
    pub(crate) fn sends(&self) -> Result<Vec<(Watch, Option<String>)>> {
        let mut out = Vec::new();
        for watch in self.watches()? {
            if matches!(watch.kind, WatchKind::LnSend | WatchKind::Withdraw) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::*;

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
                    "ALTER TABLE watches DROP COLUMN operation;
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
