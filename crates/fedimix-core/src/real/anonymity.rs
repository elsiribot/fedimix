//! The anonymity estimates: one task per connection copies the deposits of each
//! completed session into the store, and the estimates are counted from there,
//! so they need no network and keep their values while the sync is stuck.

use std::sync::Arc;
use std::time::Duration;

use super::Core;
use crate::anonymity::{self, FEDERATION_WINDOW_SESSIONS, Own};
use crate::api::{Amount, HistoryStatus};
use crate::error::Result;
use crate::event::Event;
use crate::federation::FederationClient;
use crate::scheduler::{DENOMINATIONS_SAT, Origin};

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// The smallest denomination: the federation estimate counts deposits of at
/// least this.
const SMALLEST_DENOMINATION: Amount = Amount {
    msat: DENOMINATIONS_SAT[DENOMINATIONS_SAT.len() - 1] * 1_000,
};

/// The first session the sync of `federation` fetched, kept so that the number
/// of synced sessions survives a restart.
fn sync_start_name(federation: &str) -> String {
    format!("anon-start:{federation}")
}

/// How many sessions of a backlog are synced between two events, so a long
/// one shows progress.
const SESSIONS_PER_EVENT: u64 = 100;

fn next_backoff(backoff: Duration) -> Duration {
    (backoff * 2).min(MAX_BACKOFF)
}

/// A history row id that names an outpoint, as v2 deposit rows do.
fn is_outpoint(id: &str) -> bool {
    id.parse::<bitcoin::OutPoint>().is_ok()
}

impl Core {
    /// This wallet's deposits at `federation`, which no estimate counts.
    ///
    /// On chain by outpoint (v2 rows) or txid (v1 rows). A txid also leaves
    /// out anyone else's deposit in the same funding transaction, which errs
    /// low.
    ///
    /// Lightning from the moment the invoice is made: its contract can land in
    /// a session before the receive settles. A settled receive is placed in
    /// the session count read when it settled, by its contract amount. A
    /// receive seen late (after a restart, say) is placed in a later session
    /// than the one that accepted it, so a window that starts between the two
    /// subtracts it without holding it: the count errs low, never high. One
    /// with no session (pending, or its count could not be read) is
    /// subtracted from every window that ends after it was made, by the
    /// amount on its row, which for a pending one is the invoice's (above the
    /// contract): low again. All windows end at the newest synced session, and
    /// the time that session was written stands for when it ended.
    pub(super) fn own_deposits(&self, federation: &str) -> Result<Own> {
        let store = self.store();
        let mut own = Own::default();
        for row in store.deposit_rows(federation)? {
            if is_outpoint(&row.id) {
                own.outpoints.insert(row.id);
            } else if let Some(txid) = row.txid {
                own.txids.insert(txid);
            }
        }
        // With nothing synced there is no window; until a sync writes its
        // time, every receive counts as made before the window ended.
        let window_end = store.session_synced_at(federation)?;
        for row in store.lightning_receive_rows(federation)? {
            if matches!(
                row.status,
                HistoryStatus::Expired | HistoryStatus::Failed | HistoryStatus::Refunded
            ) {
                continue;
            }
            match row.session {
                Some(session) => own.lightning.push((session, row.amount)),
                None if window_end.is_none_or(|end| row.timestamp <= end) => {
                    own.lightning_unplaced.push(row.amount);
                }
                None => {}
            }
        }
        Ok(own)
    }

    /// The anonymity estimate of an item: other people's deposits of at least
    /// `denomination` synced so far in sessions after its origin.
    pub(super) fn item_estimate(
        &self,
        federation: &str,
        origin: Origin,
        denomination: Amount,
    ) -> Result<u32> {
        let own = self.own_deposits(federation)?;
        let deposits = self.store().session_deposits(federation, origin.session)?;
        Ok(anonymity::estimate(
            &deposits,
            origin.session,
            denomination,
            &own,
        ))
    }

    /// The first session synced and the next one to sync, once one was.
    pub(super) fn synced_range(&self, federation: &str) -> Result<Option<(u64, u64)>> {
        let store = self.store();
        let Some(cursor) = store.session_cursor(federation)? else {
            return Ok(None);
        };
        let start = match store.cursor(&sync_start_name(federation))? {
            Some(start) => start,
            None => store
                .federations()?
                .into_iter()
                .find(|row| row.id == federation)
                .and_then(|row| row.joined_session)
                .unwrap_or(cursor),
        };
        Ok(Some((start.min(cursor), cursor)))
    }

    fn counted_federation_estimate(&self, federation: &str) -> Result<u32> {
        let Some((start, cursor)) = self.synced_range(federation)? else {
            return Ok(0);
        };
        let synced = cursor - start;
        let window = synced.min(FEDERATION_WINDOW_SESSIONS);
        if window == 0 {
            return Ok(0);
        }
        let own = self.own_deposits(federation)?;
        let deposits = self
            .store()
            .session_deposits_from(federation, cursor - window)?;
        let rate =
            anonymity::per_session(&deposits, cursor - 1, synced, SMALLEST_DENOMINATION, &own);
        // A whole count divided by the window, so this is exact.
        Ok((rate * window as f64).round() as u32)
    }

    /// Other people's deposits of at least 0.001 BTC in the federation's last
    /// 480 synced sessions. Read from the store, so a sync that is stuck leaves
    /// it as it was; 0 if the store cannot be read.
    pub(super) fn federation_estimate(&self, federation: &str) -> u32 {
        self.counted_federation_estimate(federation)
            .unwrap_or_else(|e| {
                eprintln!("fedimix: counting the deposits of federation {federation}: {e}");
                0
            })
    }

    /// The next session to fetch. The first time, that is the session of the
    /// join, else the oldest origin of a queued item, else the current one; it
    /// is written down, so a restart counts from the same place.
    async fn next_session(&self, client: &dyn FederationClient, federation: &str) -> Result<u64> {
        let (cursor, start, joined, oldest) = {
            let store = self.store();
            let joined = store
                .federations()?
                .into_iter()
                .find(|row| row.id == federation)
                .and_then(|row| row.joined_session);
            let oldest = store
                .exit_items(Some(federation))?
                .iter()
                .map(|item| item.origin.session)
                .min();
            (
                store.session_cursor(federation)?,
                store.cursor(&sync_start_name(federation))?,
                joined,
                oldest,
            )
        };
        if let Some(next) = cursor.or(start) {
            return Ok(next);
        }
        let start = match joined.or(oldest) {
            Some(start) => start,
            None => client.session_count().await?,
        };
        self.store()
            .set_cursor(&sync_start_name(federation), start)?;
        Ok(start)
    }

    /// Fetches and stores every completed session not yet synced, in order,
    /// and stops at the first that fails. Tells subscribers at the end, and
    /// every [`SESSIONS_PER_EVENT`] sessions of a long backlog.
    async fn sync_sessions(&self, client: &dyn FederationClient, federation: &str) -> Result<()> {
        let next = self.next_session(client, federation).await?;
        let count = client.session_count().await?;
        self.note_session_count(federation, count);
        if next >= count {
            return Ok(());
        }
        let mut before = self.federation_estimate(federation);
        let mut synced = Ok(());
        let mut stored = 0u64;
        for session in next..count {
            let fetched = match client.session_deposits(session).await {
                Ok(Some(deposits)) => deposits,
                Ok(None) => break,
                Err(e) => {
                    synced = Err(e);
                    break;
                }
            };
            let at = (self.clock)();
            if let Err(e) = self.store().add_session(federation, session, &fetched, at) {
                synced = Err(e);
                break;
            }
            stored += 1;
            if stored.is_multiple_of(SESSIONS_PER_EVENT) {
                before = self.announce_sessions(federation, before);
            }
        }
        if !stored.is_multiple_of(SESSIONS_PER_EVENT) {
            self.announce_sessions(federation, before);
        }
        synced
    }

    /// Tells subscribers that sessions were synced, and whether the federation
    /// estimate moved from `before`. Returns the estimate now.
    fn announce_sessions(&self, federation: &str, before: u32) -> u32 {
        self.events.emit(&Event::QueueChanged);
        let now = self.federation_estimate(federation);
        if now != before {
            self.events.emit(&Event::FederationChanged);
        }
        now
    }
}

/// Keeps the federation's sessions synced while its connection lasts. A
/// session that fails is tried again, never skipped, after 1 s, doubling to
/// 5 min; counting stops there until it works.
pub(super) async fn follow_sessions(
    core: Arc<Core>,
    client: Arc<dyn FederationClient>,
    id: String,
) {
    let poll = Duration::from_secs(core.timing.session_poll_secs);
    let mut backoff = FIRST_BACKOFF;
    loop {
        let wait = match core.sync_sessions(&*client, &id).await {
            Ok(()) => {
                backoff = FIRST_BACKOFF;
                poll
            }
            Err(e) => {
                eprintln!("fedimix: syncing the sessions of federation {id}: {e}");
                let wait = backoff;
                backoff = next_backoff(backoff);
                wait
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// [`Core::item_estimate`] for an item whose money arrived in `origin_session`.
#[cfg(feature = "testing")]
pub fn item_estimate(
    backend: &super::RealBackend,
    federation: &str,
    origin_session: u64,
    denomination: Amount,
) -> Result<u32> {
    let origin = Origin {
        session: origin_session,
        at: 0,
    };
    backend.core.item_estimate(federation, origin, denomination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_back_off_from_a_second_to_five_minutes() {
        let mut backoff = FIRST_BACKOFF;
        let mut seen = Vec::new();
        for _ in 0..11 {
            seen.push(backoff.as_secs());
            backoff = next_backoff(backoff);
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
    }

    #[test]
    fn only_v2_rows_are_named_by_outpoint() {
        assert!(is_outpoint(&format!("{:064x}:1", 7)));
        assert!(!is_outpoint("deposit-3"));
        assert!(!is_outpoint(&format!("{:064x}", 7)));
    }
}
