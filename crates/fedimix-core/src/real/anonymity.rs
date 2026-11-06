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

fn next_backoff(backoff: Duration) -> Duration {
    (backoff * 2).min(MAX_BACKOFF)
}

/// A history row id that names an outpoint, as v2 deposit rows do.
fn is_outpoint(id: &str) -> bool {
    id.parse::<bitcoin::OutPoint>().is_ok()
}

impl Core {
    /// This wallet's deposits at `federation`, which no estimate counts: on
    /// chain by outpoint (v2 rows) or txid (v1 rows), Lightning by session and
    /// contract amount, which is what a settled receive's row holds.
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
        for row in store.lightning_receive_rows(federation)? {
            if row.status == HistoryStatus::Complete
                && let Some(session) = row.session
            {
                own.lightning.push((session, row.amount));
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
    fn synced_range(&self, federation: &str) -> Result<Option<(u64, u64)>> {
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
    /// and stops at the first that fails. Tells subscribers once at the end.
    async fn sync_sessions(&self, client: &dyn FederationClient, federation: &str) -> Result<()> {
        let next = self.next_session(client, federation).await?;
        let count = client.session_count().await?;
        if next >= count {
            return Ok(());
        }
        let before = self.federation_estimate(federation);
        let mut synced = Ok(());
        let mut stored = false;
        for session in next..count {
            let fetched = match client.session_deposits(session).await {
                Ok(Some(deposits)) => deposits,
                Ok(None) => break,
                Err(e) => {
                    synced = Err(e);
                    break;
                }
            };
            if let Err(e) = self.store().add_session(federation, session, &fetched) {
                synced = Err(e);
                break;
            }
            stored = true;
        }
        if stored {
            self.events.emit(&Event::QueueChanged);
            if self.federation_estimate(federation) != before {
                self.events.emit(&Event::FederationChanged);
            }
        }
        synced
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
