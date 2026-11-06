//! The exit queue: the money above Spending's target, booked in denominations
//! on their way to Saving. It grows after each balance change, and gives way
//! newest first only to a confirmed Spending send and to a higher target
//! (Phase 1 ruling O1). Each change reads and writes the store under one hold
//! of its lock, so two changes never book the same money.

use super::Core;
use super::send::{fresh_id, is_send};
use crate::anonymity::{
    self, DEFAULT_SESSION_SECS, FEDERATION_WINDOW_SESSIONS, Own, SessionDeposit,
};
use crate::api::*;
use crate::error::Result;
use crate::event::Event;
use crate::scheduler::{self, Booked, Origin};
use crate::store::{ExitItem, ExitState, Store, WatchKind};

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
    /// its balance may not show yet. Read with the store held, as `held`
    /// shows: a send reserves before it writes its row and fits the queue
    /// under that lock, so a change made under it sees the send whole or not
    /// at all.
    fn reserved(&self, held: &Store, federation: &str) -> Amount {
        let _ = held;
        let submitting = self.submitting.borrow();
        sum(submitting
            .values()
            .filter(|(of, _)| of == federation)
            .map(|(_, taken)| *taken))
    }

    /// `None` when `federation` has no send left unfinished. Otherwise what
    /// those of them that have no operation and that no call is submitting
    /// will take: a crash between a send's row and its fit leaves such a
    /// send, which counts until the resolver settles it.
    fn unresolved_sends(&self, store: &Store, federation: &str) -> Result<Option<Amount>> {
        let submitting = self.submitting.borrow();
        let mut unresolved = None;
        for (watch, operation) in store.sends()? {
            if !is_send(watch.kind) || watch.federation_id != federation {
                continue;
            }
            let mut taking = unresolved.unwrap_or(Amount::ZERO);
            if operation.is_none()
                && !submitting.contains_key(&watch.id)
                && let Some(row) =
                    store.history_item(WalletKind::Spending, Some(federation), &watch.id)?
            {
                taking = sum([taking, row.amount, row.fee.unwrap_or(Amount::ZERO)]);
            }
            unresolved = Some(taking);
        }
        Ok(unresolved)
    }

    /// What `federation`'s exits with no operation, which no call is
    /// submitting, may still take: a crash between an exit's start and its
    /// submission leaves such an exit, whose money the balance may still
    /// hold until the resolver settles it. So may an exit whose outcome is
    /// unknown, which stays on its way with no watch until it lands. The
    /// queue must not grow onto either.
    fn unresolved_exits(&self, store: &Store, federation: &str) -> Result<Amount> {
        let submitting = self.submitting.borrow();
        let mut taking = Amount::ZERO;
        let mut open = std::collections::HashSet::new();
        for (watch, operation) in store.sends()? {
            if watch.kind != WatchKind::Exit || watch.federation_id != federation {
                continue;
            }
            open.insert(watch.id.clone());
            if operation.is_some() || submitting.contains_key(&watch.id) {
                continue;
            }
            if let Some(item) = store.exit_by_intent(&watch.id)? {
                let fee = item.fee.unwrap_or(Amount::ZERO);
                taking = sum([taking, item.denomination, fee]);
            }
        }
        for item in store.exit_items(Some(federation))? {
            let lost = item.state == ExitState::Withdrawing
                && item.txid.is_none()
                && item.intent.as_ref().is_some_and(|intent| {
                    !open.contains(intent) && !submitting.contains_key(intent)
                });
            if lost {
                let fee = item.fee.unwrap_or(Amount::ZERO);
                taking = sum([taking, item.denomination, fee]);
            }
        }
        Ok(taking)
    }

    /// The queued items of `federation` other than `item`, read with the
    /// store held: what an exit of `item` must leave in the balance.
    pub(super) fn queued_beside(
        &self,
        store: &Store,
        federation: &str,
        item: &str,
    ) -> Result<Amount> {
        let items = store.exit_items(Some(federation))?;
        Ok(sum(items
            .iter()
            .filter(|other| other.state == ExitState::Queued && other.id != item)
            .map(|other| other.denomination)))
    }

    /// What of `balance` is for spending: neither queued nor taken by a
    /// submission. An exit's fee comes from it.
    pub(super) fn exit_fee_room(&self, federation: &str, balance: Amount) -> Result<Amount> {
        let store = self.store();
        let queued = queued_sum(&store.exit_items(Some(federation))?);
        Ok(balance
            .saturating_sub(self.reserved(&store, federation))
            .saturating_sub(queued))
    }

    /// Books the whole denominations above the target of `federation`'s
    /// stored balance, and keeps the queue within that balance. Money a
    /// submission is about to take is not booked, nor what an exit cut off
    /// before its submission may still take. While a send is unfinished
    /// the queue keeps to what it leaves above the target, so a send whose fit
    /// was cut off still takes its money from the queue. New items start where
    /// the latest deposit came in, else at `session_count` and now. Everything
    /// is read under one hold of the store's lock. Whether the queue changed.
    pub(super) fn tend_queue(&self, federation: &str, session_count: u64) -> Result<bool> {
        let now = (self.clock)();
        let store = self.store();
        let Some(row) = store
            .federations()?
            .into_iter()
            .find(|row| row.id == federation)
        else {
            return Ok(false);
        };
        let balance = Amount {
            msat: row.balance_msat,
        };
        let target = store.settings()?.spending_target;
        let reserved = self.reserved(&store, federation);
        let cap = match self.unresolved_sends(&store, federation)? {
            Some(unresolved) => balance.saturating_sub(unresolved).saturating_sub(target),
            None => balance,
        };
        let queued = queued_sum(&store.exit_items(Some(federation))?);
        if queued > cap {
            return fit_in(&store, federation, cap, now);
        }
        let free = balance
            .saturating_sub(reserved)
            .saturating_sub(self.unresolved_exits(&store, federation)?);
        let grown = scheduler::grow(queued, free, target);
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
        let store = self.store();
        let target = store.settings()?.spending_target;
        let cap = balance
            .saturating_sub(self.reserved(&store, federation))
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

    /// After a send's row is written: the queue of `federation` shrinks,
    /// newest first, to what stays above the target once `balance` pays every
    /// submission, this send's included.
    pub(super) fn fit_queue_after_send(&self, federation: &str, balance: Amount) -> Result<()> {
        let now = (self.clock)();
        let changed = {
            let store = self.store();
            let target = store.settings()?.spending_target;
            let cap = balance
                .saturating_sub(self.reserved(&store, federation))
                .saturating_sub(target);
            fit_in(&store, federation, cap, now)?
        };
        if changed {
            self.events.emit(&Event::QueueChanged);
        }
        Ok(())
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
                .saturating_sub(self.reserved(store, &row.id))
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
            match self.tend_queue(&row.id, count) {
                Ok(tended) => changed |= tended,
                Err(e) => eprintln!("fedimix: filling the queue of federation {}: {e}", row.id),
            }
        }
        changed
    }

    /// The session count last read on this connection. The sync cursor is no
    /// stand-in: it can lag far behind, and too old an origin would overstate
    /// a new item's anonymity.
    pub(super) fn known_session_count(&self, federation: &str) -> Option<u64> {
        let state = self.state();
        let seen = state
            .live
            .get(federation)
            .and_then(|live| live.sessions_seen);
        seen.map(|seen| seen.last.1)
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

    /// The active federation's queue, each queued item with the status the
    /// scheduler sees for it.
    pub(super) fn queue_state(&self) -> Result<QueueState> {
        let waiting_for_saving = !self.saving_usable();
        let (federation, settings, items, balance) = {
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
            let balance = store
                .federations()?
                .into_iter()
                .find(|row| row.id == federation)
                .map_or(Amount::ZERO, |row| Amount {
                    msat: row.balance_msat,
                });
            (federation, store.settings()?, items, balance)
        };
        let for_spending = self.exit_fee_room(&federation, balance)?;
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
                    status: self.queued_status(item, &settings, for_spending),
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
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::config::CoreConfig;
    use crate::federation::fake::FakePort;

    /// I2: a tend blocked on the store while a send reserves its money must
    /// see that reservation once it runs, and book none of it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tend_sees_a_reservation_made_while_it_waited() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let port = Arc::new(FakePort::default());
        let client = port.add_federation("fed1one", "one", Network::Regtest);
        client.set_balance(Amount::from_sat(47_000_000));
        let config = CoreConfig {
            data_dir: dir.path().to_str().unwrap().to_string(),
            network: Network::Regtest,
        };
        let backend = crate::testing::real_backend(config, port, Arc::new(|| 1_000))
            .await
            .unwrap();
        let info = crate::Backend::join_federation(&*backend, "fed1one".into())
            .await
            .unwrap();
        let core = Arc::clone(&backend.core);
        let full = Amount::from_sat(42_000_000);
        tokio::time::timeout(Duration::from_secs(10), async {
            while core.queued_total(&info.id).unwrap() != full {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        // The tend starts while the store is held, and the send reserves
        // before it is let go.
        let (tending, reservation) = {
            let store = core.store();
            store
                .set_federation_balance(&info.id, Amount::from_sat(50_100_000).msat)
                .unwrap();
            let tending = {
                let core = Arc::clone(&core);
                let id = info.id.clone();
                std::thread::spawn(move || core.tend_queue(&id, 0).unwrap())
            };
            std::thread::sleep(Duration::from_millis(100));
            let reservation = core
                .reserve(
                    &info.id,
                    "send-x",
                    Amount::from_sat(3_100_000),
                    Amount::from_sat(50_100_000),
                )
                .unwrap();
            (tending, reservation)
        };
        assert!(!tending.join().unwrap(), "nothing above the target is free");
        assert_eq!(core.queued_total(&info.id).unwrap(), full);
        drop(reservation);
        crate::Backend::shutdown(&*backend).await.unwrap();
    }

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
