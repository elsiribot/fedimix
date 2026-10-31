//! SQLite persistence: settings, federations, history, watches and cursors.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::api::{HistoryItem, Settings, Timestamp, WalletKind};
use crate::error::{FedimixError, Result};

const SCHEMA_VERSION: i64 = 1;

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
CREATE INDEX history_order ON history (wallet, timestamp DESC, id DESC);
CREATE TABLE watches (
    id TEXT PRIMARY KEY,
    federation_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    finished INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE cursors (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
";

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
pub(crate) enum WatchKind {
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

/// An operation to follow until it settles, kept across restarts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Watch {
    pub id: String,
    pub federation_id: String,
    pub kind: WatchKind,
    pub created_at: Timestamp,
}

/// Synchronous; callers wrap it in a `Mutex` and keep calls short.
#[derive(Debug)]
pub(crate) struct Store {
    conn: Connection,
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path).map_err(db_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err)?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let has_version: bool = self
            .conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version')",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if !has_version {
            // Create everything or nothing.
            let tx = self.conn.unchecked_transaction().map_err(db_err)?;
            tx.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL);")
                .map_err(db_err)?;
            tx.execute_batch(SCHEMA).map_err(db_err)?;
            tx.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [SCHEMA_VERSION],
            )
            .map_err(db_err)?;
            return tx.commit().map_err(db_err);
        }
        let version: i64 = self
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .map_err(db_err)?;
        if version > SCHEMA_VERSION {
            return Err(FedimixError::Internal(
                "This data folder was made by a newer version of Fedimix. Update Fedimix to open it."
                    .into(),
            ));
        }
        // Migrations from older versions go here; version 1 is the first.
        Ok(())
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
    pub(crate) fn add_federation(&self, row: &FederationRow) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO federations (id, name, invite, online_at, balance_msat, position)
                 VALUES (?1, ?2, ?3, ?4, ?5, (SELECT COALESCE(MAX(position), 0) + 1 FROM federations))
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

    pub(crate) fn active_federation(&self) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT id FROM federations WHERE active = 1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_err)
    }

    pub(crate) fn set_active(&self, id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
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
    ) -> Result<()> {
        let wallet = wallet_name(wallet);
        let federation_id = federation_id.unwrap_or("");
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT timestamp FROM history WHERE wallet = ?1 AND federation_id = ?2 AND id = ?3",
                params![wallet, federation_id, item.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        let mut item = item.clone();
        if let Some(ts) = existing {
            item.timestamp = from_db(ts);
        }
        let json = serde_json::to_string(&item).map_err(db_err)?;
        tx.execute(
            "INSERT INTO history (wallet, federation_id, id, timestamp, json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(wallet, federation_id, id) DO UPDATE SET json = excluded.json",
            params![wallet, federation_id, item.id, to_db(item.timestamp), json],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }

    /// Newest first by `(timestamp, id)`. `before` is the id of the last item
    /// the caller has; rows that arrive later never shift the next page.
    pub(crate) fn history_page(
        &self,
        wallet: WalletKind,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HistoryItem>> {
        let wallet = wallet_name(wallet);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let cursor: Option<(i64, String)> = match before {
            None => None,
            Some(id) => Some(
                self.conn
                    .query_row(
                        "SELECT timestamp, id FROM history WHERE wallet = ?1 AND id = ?2 LIMIT 1",
                        params![wallet, id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(db_err)?
                    .ok_or_else(|| FedimixError::UnknownItem(id.to_string()))?,
            ),
        };
        let (ts, id) = cursor.unwrap_or((i64::MAX, String::new()));
        // Without a cursor everything qualifies: ts is the maximum, and the
        // id comparison is skipped by the `?4` flag.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT json FROM history
                 WHERE wallet = ?1 AND (?4 = 0 OR (timestamp, id) < (?2, ?3))
                 ORDER BY timestamp DESC, id DESC LIMIT ?5",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![wallet, ts, id, before.is_some(), limit], |r| {
                r.get::<_, String>(0)
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json.map_err(db_err)?).map_err(db_err)?);
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

    pub(crate) fn add_watch(&self, watch: &Watch) -> Result<()> {
        self.conn
            .execute(
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

    /// Finishing an unknown id does nothing.
    pub(crate) fn finish_watch(&self, id: &str) -> Result<()> {
        self.conn
            .execute("UPDATE watches SET finished = 1 WHERE id = ?1", [id])
            .map_err(db_err)?;
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
            ids(&store.history_page(WalletKind::Spending, None, 10).unwrap()),
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
        let first = store.history_page(WalletKind::Spending, None, 2).unwrap();
        assert_eq!(ids(&first), ["e", "d"]);
        store
            .upsert_history(WalletKind::Spending, Some("f"), &item("new", 100))
            .unwrap();
        let second = store
            .history_page(WalletKind::Spending, Some("d"), 2)
            .unwrap();
        assert_eq!(ids(&second), ["c", "b"]);
        let third = store
            .history_page(WalletKind::Spending, Some("b"), 5)
            .unwrap();
        assert_eq!(ids(&third), ["a"]);
        assert!(
            store
                .history_page(WalletKind::Saving, None, 5)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.history_page(WalletKind::Spending, Some("nope"), 5),
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
        let page = store.history_page(WalletKind::Spending, None, 10).unwrap();
        assert_eq!(ids(&page), ["b", "a"]);
        assert_eq!(page[1].status, HistoryStatus::Complete);
        assert_eq!(page[1].timestamp, 1);
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

    #[test]
    fn set_active_unknown_is_refused() {
        let (_d, store) = open();
        assert_eq!(
            store.set_active("zzz"),
            Err(crate::FedimixError::UnknownItem("zzz".into()))
        );
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
