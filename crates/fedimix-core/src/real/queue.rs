//! The exit queue: the money above Spending's target, booked in denominations
//! on their way to Saving. It grows after each balance change, and gives way
//! newest first only to a confirmed Spending send and to a higher target
//! (Phase 1 ruling O1). Each change reads and writes the store under one hold
//! of its lock, so two changes never book the same money.

use super::Core;
use super::send::fresh_id;
use crate::anonymity::{
    self, DEFAULT_SESSION_SECS, FEDERATION_WINDOW_SESSIONS, Own, SessionDeposit,
};
use crate::api::*;
use crate::error::Result;
use crate::event::Event;
use crate::scheduler::{self, Booked, Origin};
use crate::store::{ExitItem, ExitState, Store};

/// How many sessions must pass on a connection before their length counts
/// as measured.
const MEASURED_AFTER_SESSIONS: u64 = 10;

/// The first and the latest session count read on one connection, each with
/// when it was read: the session length is measured between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SessionsSeen {
    first: (Timestamp, u64),
    last: (Timestamp, u64),
}

impl SessionsSeen {
    pub(super) fn note(seen: &mut Option<Self>, at: Timestamp, count: u64) {
        match seen {
            Some(seen) => seen.last = (at, count),
            None => {
                *seen = Some(Self {
                    first: (at, count),
                    last: (at, count),
                });
            }
        }
    }

    fn session_secs(&self) -> Option<u64> {
        let passed = self.last.1.saturating_sub(self.first.1);
        (passed >= MEASURED_AFTER_SESSIONS)
            .then(|| self.last.0.saturating_sub(self.first.0) / passed)
    }
}

fn sum(amounts: impl IntoIterator<Item = Amount>) -> Amount {
    Amount {
        msat: amounts
            .into_iter()
            .fold(0u64, |total, amount| total.saturating_add(amount.msat)),
    }
}

/// The items `scheduler::fit` may move: the queued ones, never one that is
/// withdrawing.
fn booked(items: &[ExitItem]) -> Vec<Booked> {
    items
        .iter()
        .filter(|item| item.state == ExitState::Queued)
        .map(|item| Booked {
            id: item.id.clone(),
            denomination: item.denomination,
            origin: item.origin,
        })
        .collect()
}

fn queued_sum(items: &[ExitItem]) -> Amount {
    sum(items
        .iter()
        .filter(|item| item.state == ExitState::Queued)
        .map(|item| item.denomination))
}

fn new_items(
    federation: &str,
    denominations: &[Amount],
    origin: Origin,
    now: Timestamp,
) -> Vec<ExitItem> {
    denominations
        .iter()
        .map(|&denomination| ExitItem {
            id: fresh_id("queue"),
            federation_id: federation.to_string(),
            denomination,
            origin,
            created_at: now,
            state: ExitState::Queued,
            intent: None,
            address: None,
            txid: None,
            fee: None,
            estimate: None,
            early: false,
        })
        .collect()
}

/// Where new money of `federation` came in: its latest settled deposit or
/// Lightning receive that knows its session.
fn latest_origin(store: &Store, federation: &str) -> Result<Option<Origin>> {
    let mut rows = store.deposit_rows(federation)?;
    rows.extend(store.lightning_receive_rows(federation)?);
    Ok(rows
        .into_iter()
        .filter(|row| row.status == HistoryStatus::Complete)
        .filter_map(|row| {
            row.session.map(|session| Origin {
                session,
                at: row.timestamp,
            })
        })
        .max_by_key(|origin| (origin.session, origin.at)))
}

/// Shrinks the queued items of `federation` to at most `cap`, newest first,
/// and books again what that freed up to the cap. Whether anything changed.
fn fit_in(store: &Store, federation: &str, cap: Amount, now: Timestamp) -> Result<bool> {
    let fit = scheduler::fit(&booked(&store.exit_items(Some(federation))?), cap);
    let Some(origin) = fit.origin else {
        return Ok(false);
    };
    let regrown = new_items(federation, &fit.regrow, origin, now);
    store.replace_queued_exits(&fit.removed, &regrown)?;
    Ok(true)
}

/// What the anonymity figures of one federation's items are counted from,
/// read from the store once per snapshot.
struct Counting {
    own: Own,
    deposits: Vec<(u64, SessionDeposit)>,
    /// The newest synced session.
    head: u64,
    synced: u64,
}

impl Counting {
    fn estimate(&self, origin: Origin, denomination: Amount) -> u32 {
        anonymity::estimate(&self.deposits, origin.session, denomination, &self.own)
    }

    fn per_session(&self, denomination: Amount) -> f64 {
        anonymity::per_session(
            &self.deposits,
            self.head,
            self.synced,
            denomination,
            &self.own,
        )
    }
}

impl Core {
    /// Saving can take an exit: set up, its wallet open, its first scan done.
    pub(super) fn saving_usable(&self) -> bool {
        let live = self.saving();
        live.is_set_up() && live.wallet().is_some_and(|wallet| wallet.first_scan_done())
    }

    /// What the payments being submitted from `federation` will take, which
    /// its balance may not show yet.
    fn reserved(&self, federation: &str) -> Amount {
        let submitting = self.submitting.borrow();
        sum(submitting
            .values()
            .filter(|(of, _)| of == federation)
            .map(|(_, taken)| *taken))
    }

    /// Books the whole denominations above the target, and keeps the queue
    /// within `balance`. Money a submission is about to take is not booked.
    /// New items start where the latest deposit came in, else at
    /// `session_count` and now. Whether the queue changed.
    pub(super) fn tend_queue(
        &self,
        federation: &str,
        balance: Amount,
        session_count: u64,
    ) -> Result<bool> {
        let reserved = self.reserved(federation);
        let now = (self.clock)();
        let store = self.store();
        let target = store.settings()?.spending_target;
        let queued = queued_sum(&store.exit_items(Some(federation))?);
        if queued > balance {
            return fit_in(&store, federation, balance, now);
        }
        let grown = scheduler::grow(queued, balance.saturating_sub(reserved), target);
        if grown.is_empty() {
            return Ok(false);
        }
        let origin = latest_origin(&store, federation)?.unwrap_or(Origin {
            session: session_count,
            at: now,
        });
        store.add_exit_items(&new_items(federation, &grown, origin, now))?;
        Ok(true)
    }

    /// The money of `federation` still queued: what Spending shows as moving.
    pub(super) fn queued_total(&self, federation: &str) -> Result<Amount> {
        Ok(queued_sum(&self.store().exit_items(Some(federation))?))
    }

    /// How much less the queue would hold after a debit of `debit` from
    /// `balance`, beside the payments already being submitted.
    pub(super) fn queue_shrink_for(
        &self,
        federation: &str,
        debit: Amount,
        balance: Amount,
    ) -> Result<Amount> {
        let reserved = self.reserved(federation);
        let store = self.store();
        let target = store.settings()?.spending_target;
        let cap = balance
            .saturating_sub(reserved)
            .saturating_sub(debit)
            .saturating_sub(target);
        let items = store.exit_items(Some(federation))?;
        let fit = scheduler::fit(&booked(&items), cap);
        let removed = sum(items
            .iter()
            .filter(|item| fit.removed.contains(&item.id))
            .map(|item| item.denomination));
        Ok(removed.saturating_sub(sum(fit.regrow.iter().copied())))
    }

    /// Shrinks the queue of `federation` to at most `cap`, newest first.
    pub(super) fn fit_queue(&self, federation: &str, cap: Amount) -> Result<()> {
        let now = (self.clock)();
        if fit_in(&self.store(), federation, cap, now)? {
            self.events.emit(&Event::QueueChanged);
        }
        Ok(())
    }

    /// After a send's row is written: the queue keeps what stays above the
    /// target once `balance` pays every submission, this send's included.
    pub(super) fn fit_queue_after_send(&self, federation: &str, balance: Amount) -> Result<()> {
        let reserved = self.reserved(federation);
        let target = self.store().settings()?.spending_target;
        let cap = balance.saturating_sub(reserved).saturating_sub(target);
        self.fit_queue(federation, cap)
    }

    /// A higher target: every federation's queue gives way to it. Whether
    /// any queue changed.
    pub(super) fn fit_queues_to(&self, store: &Store, target: Amount) -> Result<bool> {
        let now = (self.clock)();
        let mut changed = false;
        for row in store.federations()? {
            let balance = Amount {
                msat: row.balance_msat,
            };
            let cap = balance
                .saturating_sub(self.reserved(&row.id))
                .saturating_sub(target);
            changed |= fit_in(store, &row.id, cap, now)?;
        }
        Ok(changed)
    }

    /// A lower target: every federation's queue books what now lies above
    /// it. A federation whose session count was never read waits for its next
    /// balance change. Whether any queue changed.
    pub(super) fn tend_queues(&self) -> bool {
        let rows = match self.store().federations() {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("fedimix: reading the federations to fill their queues: {e}");
                return false;
            }
        };
        let mut changed = false;
        for row in rows {
            let Some(count) = self.known_session_count(&row.id) else {
                continue;
            };
            let balance = Amount {
                msat: row.balance_msat,
            };
            match self.tend_queue(&row.id, balance, count) {
                Ok(tended) => changed |= tended,
                Err(e) => eprintln!("fedimix: filling the queue of federation {}: {e}", row.id),
            }
        }
        changed
    }

    /// The session count last read on this connection, else the next session
    /// to sync.
    pub(super) fn known_session_count(&self, federation: &str) -> Option<u64> {
        let seen = self
            .state()
            .live
            .get(federation)
            .and_then(|live| live.sessions_seen);
        seen.map(|seen| seen.last.1)
            .or_else(|| self.store().session_cursor(federation).ok().flatten())
    }

    /// The exits on their way to Saving, of every federation.
    pub(super) fn exits_in_flight(&self) -> Result<Amount> {
        let items = self.store().exit_items(None)?;
        Ok(sum(items
            .iter()
            .filter(|item| item.state == ExitState::Withdrawing)
            .map(|item| item.denomination)))
    }

    /// The session length of `federation`, as measured on this connection
    /// once enough sessions have passed.
    fn session_secs(&self, federation: &str) -> u64 {
        let state = self.state();
        let seen = state
            .live
            .get(federation)
            .and_then(|live| live.sessions_seen);
        seen.and_then(|seen| seen.session_secs())
            .unwrap_or(DEFAULT_SESSION_SECS)
    }

    pub(super) fn note_session_count(&self, federation: &str, count: u64) {
        let now = (self.clock)();
        let mut state = self.state();
        let live = state.live.entry(federation.to_string()).or_default();
        SessionsSeen::note(&mut live.sessions_seen, now, count);
    }

    /// Everything the items' figures need, in a few reads: the own
    /// deposits, and the synced deposits from the oldest item's origin or the
    /// rate's window, whichever starts first.
    fn counting(&self, federation: &str, items: &[ExitItem]) -> Result<Counting> {
        let own = self.own_deposits(federation)?;
        let Some((start, cursor)) = self.synced_range(federation)? else {
            return Ok(Counting {
                own,
                deposits: Vec::new(),
                head: 0,
                synced: 0,
            });
        };
        let synced = cursor - start;
        let window_start = cursor - synced.min(FEDERATION_WINDOW_SESSIONS);
        let first = items
            .iter()
            .map(|item| item.origin.session.saturating_add(1))
            .min()
            .map_or(window_start, |first| first.min(window_start));
        Ok(Counting {
            own,
            deposits: self.store().session_deposits_from(federation, first)?,
            head: cursor.saturating_sub(1),
            synced,
        })
    }

    /// The active federation's queue. Until automatic exits exist, an item
    /// not on its way reads `Queued`.
    pub(super) fn queue_state(&self) -> Result<QueueState> {
        let waiting_for_saving = !self.saving_usable();
        let (federation, settings, items) = {
            let store = self.store();
            let Some(federation) = store.active_federation()? else {
                return Ok(QueueState {
                    total: Amount::ZERO,
                    anonymity_estimate: 0,
                    typical_exit_secs: 0,
                    waiting_for_saving,
                    items: Vec::new(),
                });
            };
            let items = store.exit_items(Some(&federation))?;
            (federation, store.settings()?, items)
        };
        let counting = self.counting(&federation, &items)?;
        let session_secs = self.session_secs(&federation);
        let now = (self.clock)();
        let mut attainable = Vec::new();
        let mut waits = Vec::new();
        let shown = items
            .iter()
            .map(|item| {
                if item.state != ExitState::Queued {
                    let current = item
                        .estimate
                        .unwrap_or_else(|| counting.estimate(item.origin, item.denomination));
                    return QueueItem {
                        id: item.id.clone(),
                        denomination: item.denomination,
                        status: QueueItemStatus::Withdrawing {
                            txid: item.txid.clone(),
                        },
                        anonymity_current: current,
                        anonymity_attainable: current,
                    };
                }
                let current = counting.estimate(item.origin, item.denomination);
                let in_pool = item
                    .origin
                    .at
                    .saturating_add(settings.min_time_in_pool_secs)
                    .saturating_sub(now);
                let wait =
                    scheduler::mean_wait_secs(current, settings.anonymity_target, &self.timing);
                let expected = in_pool.saturating_add(wait);
                let reachable = scheduler::attainable(
                    current,
                    counting.per_session(item.denomination),
                    expected,
                    session_secs,
                );
                attainable.push(u64::from(reachable));
                waits.push(expected);
                QueueItem {
                    id: item.id.clone(),
                    denomination: item.denomination,
                    status: QueueItemStatus::Queued,
                    anonymity_current: current,
                    anonymity_attainable: reachable,
                }
            })
            .collect();
        Ok(QueueState {
            total: queued_sum(&items),
            anonymity_estimate: u32::try_from(scheduler::median(&mut attainable))
                .unwrap_or(u32::MAX),
            typical_exit_secs: scheduler::median(&mut waits),
            waiting_for_saving,
            items: shown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_length_counts_once_ten_sessions_passed() {
        let mut seen = None;
        SessionsSeen::note(&mut seen, 1_000, 5);
        SessionsSeen::note(&mut seen, 1_900, 14);
        assert_eq!(seen.unwrap().session_secs(), None);
        SessionsSeen::note(&mut seen, 2_000, 15);
        assert_eq!(seen.unwrap().session_secs(), Some(100));
    }
}
