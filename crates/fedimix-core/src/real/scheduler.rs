//! Automatic exits: each queued item leaves for Saving at a time drawn from
//! an exponential law, whose mean shortens as its anonymity estimate rises.
//! One task ticks every `tick_secs`, and sooner on a change to the queue,
//! the settings, a balance or a connection. Draws live in memory only, so a
//! restart draws again, as do a reconnect, a settings change and a new
//! estimate. Every exit goes through [`Core::start_exit`], as an early one
//! does, so it is as safe against a crash.

use std::collections::HashMap;
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use futures::FutureExt as _;
use tokio::time::MissedTickBehavior;

use super::exit::quote_address;
use super::saving::stopping;
use super::{Core, Gate, held};
use crate::api::*;
use crate::error::{FedimixError, Result};
use crate::event::Event;
use crate::federation::{FederationClient, WithdrawQuote};
use crate::scheduler::{self, Conditions, Readiness};
use crate::store::{ExitItem, ExitState};

/// How long a fee quote stands before the scheduler asks again.
const QUOTE_TTL_SECS: u64 = 600;
/// After a start that failed, the wait before the item may try again. It
/// doubles with each failure in a row, up to [`MAX_RETRY_SECS`]: each try
/// reveals a fresh Saving address.
const FIRST_RETRY_SECS: u64 = 60;
const MAX_RETRY_SECS: u64 = 3_600;

/// A uniform draw in `[0, 1)`. Injected so tests are deterministic.
pub type Draw = Arc<dyn Fn() -> f64 + Send + Sync>;

pub(crate) fn random_draw() -> Draw {
    Arc::new(rand::random::<f64>)
}

/// When an item is due, and the estimate its delay was drawn for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Drawn {
    at: Timestamp,
    estimate: u32,
}

#[derive(Default)]
pub(super) struct Schedule {
    /// By item id, with the item's federation.
    draws: HashMap<String, (String, Drawn)>,
    /// By federation and denomination (msat): when it was quoted, and the
    /// quote.
    quotes: HashMap<(String, u64), (Timestamp, WithdrawQuote)>,
    /// By item id, with the item's federation: the starts that failed in a
    /// row, and when it may try again. Kept through a redraw.
    retries: HashMap<String, (String, u32, Timestamp)>,
}

/// The wait after the `failures`-th failed start in a row.
fn retry_secs(failures: u32) -> u64 {
    FIRST_RETRY_SECS
        .saturating_mul(1u64 << failures.saturating_sub(1).min(16))
        .min(MAX_RETRY_SECS)
}

/// The events after which a tick may decide differently.
fn wakes(event: &Event) -> bool {
    matches!(
        event,
        Event::QueueChanged
            | Event::SettingsChanged
            | Event::ConnectionChanged { .. }
            | Event::FederationChanged
            | Event::BalanceChanged { .. }
            | Event::SavingChanged
    )
}

/// Ticks until the gate stops it. A tick that fails is logged; the next one
/// tries again.
pub(super) async fn run_scheduler(core: Arc<Core>) -> Result<()> {
    let mut gate = core.gate.subscribe();
    let events = core.events.subscribe();
    let mut ticks = tokio::time::interval(Duration::from_secs(core.timing.tick_secs.max(1)));
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stopping(&mut gate) => return Ok(()),
            _ = ticks.tick() => {}
            () = core.scheduler_wake.notified() => {}
            event = events.next() => match event {
                None => return Ok(()),
                Some(event) if wakes(&event) => {}
                Some(_) => continue,
            },
        }
        // One tick answers every change so far, its own events included.
        while let Some(Some(_)) = events.next().now_or_never() {}
        tokio::select! {
            () = stopping(&mut gate) => return Ok(()),
            ticked = core.schedule_tick() => {
                if let Err(e) = ticked {
                    eprintln!("fedimix: scheduling exits: {e}");
                }
            }
        }
    }
}

impl Core {
    fn schedule(&self) -> MutexGuard<'_, Schedule> {
        held(&self.schedule)
    }

    /// A reconnect, or the federation going away: its delays are drawn
    /// again, and its fees asked again.
    pub(super) fn forget_connection(&self, federation: &str) {
        let mut schedule = self.schedule();
        schedule.draws.retain(|_, (of, _)| of != federation);
        schedule.quotes.retain(|(of, _), _| of != federation);
    }

    /// A settings change: every delay is drawn again.
    pub(super) fn forget_draws(&self) {
        self.schedule().draws.clear();
    }

    /// When `item` is due, once drawn.
    pub(super) fn drawn_at(&self, item: &str) -> Option<Timestamp> {
        self.schedule().draws.get(item).map(|(_, drawn)| drawn.at)
    }

    /// The last fee quoted for `denomination` on `federation`, however old.
    pub(super) fn quoted_fee(&self, federation: &str, denomination: Amount) -> Option<Amount> {
        let schedule = self.schedule();
        let key = (federation.to_string(), denomination.msat);
        schedule.quotes.get(&key).map(|(_, quote)| quote.fee)
    }

    /// The status of a queued item of `federation`, as the next tick sees
    /// it. The real backend never says `Eligible`: a ready item is either
    /// scheduled or about to be.
    pub(super) fn queued_status(
        &self,
        item: &ExitItem,
        settings: &Settings,
        for_spending: Amount,
    ) -> QueueItemStatus {
        let federation = &item.federation_id;
        let (online, online_since) = {
            let state = self.state();
            let live = state.live.get(federation);
            (
                live.is_some_and(|live| live.online) && *self.gate.borrow() == Gate::Open,
                live.and_then(|live| live.online_since),
            )
        };
        let conditions = Conditions {
            now: (self.clock)(),
            saving_usable: self.saving_usable(),
            online,
            online_since,
            fee: self.quoted_fee(federation, item.denomination),
            for_spending,
            settings: settings.clone(),
        };
        match scheduler::readiness(item.origin.at, item.denomination, &conditions) {
            Readiness::Waiting(status) => status,
            Readiness::Ready => self
                .drawn_at(&item.id)
                .map_or(QueueItemStatus::Queued, |at| QueueItemStatus::Scheduled {
                    at,
                }),
        }
    }

    /// Looks at every connected federation once: tends its queue, draws a
    /// delay for each ready item that has none or whose estimate changed,
    /// and starts at most one exit that is due. Ticks run one at a time.
    pub(super) async fn schedule_tick(self: &Arc<Self>) -> Result<()> {
        let _tick = self.scheduling.lock().await;
        if *self.gate.borrow() != Gate::Open {
            return Ok(());
        }
        let rows = self.store().federations()?;
        for row in rows {
            // Offline pauses it: nothing is drawn, nothing goes.
            let Ok(client) = self.connected(&row.id) else {
                continue;
            };
            let balance = Amount {
                msat: row.balance_msat,
            };
            if let Err(e) = self.tick_federation(&row.id, &client, balance).await {
                eprintln!(
                    "fedimix: scheduling the exits of federation {}: {e}",
                    row.id
                );
            }
        }
        Ok(())
    }

    async fn tick_federation(
        self: &Arc<Self>,
        federation: &str,
        client: &Arc<dyn FederationClient>,
        balance: Amount,
    ) -> Result<()> {
        let mut changed = false;
        if let Some(count) = self.known_session_count(federation) {
            changed |= self.tend_queue(federation, count)?;
        }
        let (items, settings, last_exit) = {
            let store = self.store();
            let items: Vec<ExitItem> = store
                .exit_items(Some(federation))?
                .into_iter()
                .filter(|item| item.state == ExitState::Queued)
                .collect();
            (items, store.settings()?, store.latest_exit_at(federation)?)
        };
        {
            let queued = |id: &String| items.iter().any(|item| item.id == *id);
            let mut schedule = self.schedule();
            schedule
                .draws
                .retain(|id, (of, _)| of != federation || queued(id));
            schedule
                .retries
                .retain(|id, (of, _, _)| of != federation || queued(id));
        }
        let now = (self.clock)();
        let saving_usable = self.saving_usable();
        if saving_usable {
            self.refresh_quotes(federation, client, &items, now).await;
        }
        let online_since = {
            let state = self.state();
            state
                .live
                .get(federation)
                .and_then(|live| live.online_since)
        };
        let for_spending = self.exit_fee_room(federation, balance)?;
        let not_before = scheduler::not_before(last_exit, &self.timing);
        let mut exited = false;
        for item in &items {
            let quote = self.fresh_quote(federation, item.denomination, now);
            let conditions = Conditions {
                now,
                saving_usable,
                online: true,
                online_since,
                fee: quote.as_ref().map(|quote| quote.fee),
                for_spending,
                settings: settings.clone(),
            };
            let ready = scheduler::readiness(item.origin.at, item.denomination, &conditions);
            let (Readiness::Ready, Some(quote)) = (ready, quote) else {
                continue;
            };
            let estimate = self.item_estimate(federation, item.origin, item.denomination)?;
            let mean = scheduler::mean_wait_secs(estimate, settings.anonymity_target, &self.timing);
            let drawn = self.schedule().draws.get(&item.id).map(|(_, d)| *d);
            let at = match drawn {
                Some(drawn) if drawn.estimate == estimate => drawn.at,
                _ => {
                    changed = true;
                    self.draw_for(item, now, mean, estimate)
                }
            };
            if at > now || exited {
                continue;
            }
            let failed = self.schedule().retries.get(&item.id).cloned();
            if failed.as_ref().is_some_and(|(_, _, until)| *until > now) {
                continue;
            }
            // Due, but the last exit was too recent: drawn again from when
            // the next may go.
            if let Some(next) = not_before.filter(|next| *next > now) {
                self.draw_for(item, next, mean, estimate);
                changed = true;
                continue;
            }
            match self.start_exit(item, quote, false).await {
                Ok(_) => {
                    exited = true;
                    let mut schedule = self.schedule();
                    schedule.draws.remove(&item.id);
                    schedule.retries.remove(&item.id);
                }
                Err(e) => {
                    eprintln!("fedimix: starting the exit of item {}: {e}", item.id);
                    let mut schedule = self.schedule();
                    if e == FedimixError::PreviewExpired {
                        // The fee rose: asked again before the next try.
                        let key = (federation.to_string(), item.denomination.msat);
                        schedule.quotes.remove(&key);
                    }
                    let failures = failed.map_or(1, |(_, failures, _)| failures + 1);
                    let until = now.saturating_add(retry_secs(failures));
                    let entry = (federation.to_string(), failures, until);
                    schedule.retries.insert(item.id.clone(), entry);
                }
            }
        }
        if changed {
            self.events.emit(&Event::QueueChanged);
        }
        Ok(())
    }

    /// Draws when `item` is due, counted from `from`, and keeps it.
    fn draw_for(&self, item: &ExitItem, from: Timestamp, mean: u64, estimate: u32) -> Timestamp {
        let at = from.saturating_add(scheduler::draw_delay(mean, (self.draw)()));
        self.schedule().draws.insert(
            item.id.clone(),
            (item.federation_id.clone(), Drawn { at, estimate }),
        );
        at
    }

    /// A quote younger than [`QUOTE_TTL_SECS`] for `denomination`.
    fn fresh_quote(
        &self,
        federation: &str,
        denomination: Amount,
        now: Timestamp,
    ) -> Option<WithdrawQuote> {
        let schedule = self.schedule();
        let key = (federation.to_string(), denomination.msat);
        schedule
            .quotes
            .get(&key)
            .filter(|(at, _)| now.saturating_sub(*at) < QUOTE_TTL_SECS)
            .map(|(_, quote)| quote.clone())
    }

    /// Asks again for each denomination of `items` whose quote is missing
    /// or old. The quote goes to [`quote_address`], never to a Saving
    /// address, as a v1 quote shows it to every guardian.
    async fn refresh_quotes(
        &self,
        federation: &str,
        client: &Arc<dyn FederationClient>,
        items: &[ExitItem],
        now: Timestamp,
    ) {
        let mut denominations: Vec<Amount> = items.iter().map(|item| item.denomination).collect();
        denominations.sort_unstable_by_key(|amount| amount.msat);
        denominations.dedup();
        let address = match quote_address(self.config.network) {
            Ok(address) => address,
            Err(e) => {
                eprintln!("fedimix: an address to quote exits to: {e}");
                return;
            }
        };
        for denomination in denominations {
            if self.fresh_quote(federation, denomination, now).is_some() {
                continue;
            }
            let exact = SendAmount::Exact(denomination);
            match client.withdraw_quote(&address, exact).await {
                Ok(quote) if quote.amount == denomination => {
                    let key = (federation.to_string(), denomination.msat);
                    self.schedule().quotes.insert(key, (now, quote));
                }
                Ok(_) => eprintln!("fedimix: federation {federation} quoted another amount"),
                Err(e) => eprintln!("fedimix: quoting an exit of {denomination}: {e}"),
            }
        }
    }
}

/// Ends the scheduler's wait, so it ticks now.
#[cfg(feature = "testing")]
pub fn run_scheduler_now(backend: &super::RealBackend) {
    backend.core.scheduler_wake.notify_one();
}

/// Runs one tick to its end, as the scheduler would.
#[cfg(feature = "testing")]
pub async fn tick_scheduler(backend: &super::RealBackend) -> Result<()> {
    backend.core.schedule_tick().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_wait_a_minute_doubling_to_an_hour() {
        let waits: Vec<u64> = (1..=8).map(retry_secs).collect();
        assert_eq!(waits, [60, 120, 240, 480, 960, 1_920, 3_600, 3_600]);
        assert_eq!(retry_secs(u32::MAX), 3_600);
    }
}
