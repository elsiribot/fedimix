//! The exit queue's rules: denominations, how a send shrinks the queue, the
//! wait and delay maths and when an item may leave. Pure: the caller passes
//! the time and the random draws.

use crate::api::{Amount, DeferReason, QueueItemStatus, Settings, Timestamp};

/// Spec section 1: the 1-2-5 series from 1 BTC down to 0.001 BTC.
pub(crate) const DENOMINATIONS_SAT: [u64; 10] = [
    100_000_000,
    50_000_000,
    20_000_000,
    10_000_000,
    5_000_000,
    2_000_000,
    1_000_000,
    500_000,
    200_000,
    100_000,
];

/// The exit clock's constants. `FAST` is for recorded demos and tests on a
/// live federation, where hours would not fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitTiming {
    /// The mean wait once an item has reached the anonymity target.
    pub short_wait_secs: u64,
    /// The longest mean wait, however few deposits came after.
    pub max_wait_secs: u64,
    /// The least time between two automatic exits of one federation.
    pub spacing_secs: u64,
    pub tick_secs: u64,
    pub session_poll_secs: u64,
}

impl ExitTiming {
    pub const DEFAULT: Self = ExitTiming {
        short_wait_secs: 10_800,
        max_wait_secs: 2_592_000,
        spacing_secs: 3_600,
        tick_secs: 30,
        session_poll_secs: 30,
    };
    pub const FAST: Self = ExitTiming {
        short_wait_secs: 20,
        max_wait_secs: 120,
        spacing_secs: 30,
        tick_secs: 2,
        session_poll_secs: 2,
    };
}

/// Where a queued item's money entered the federation: the anonymity count
/// starts after `session`, the minimum time in the pool runs from `at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    pub session: u64,
    pub at: Timestamp,
}

/// A `Queued` item as `fit` sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Booked {
    pub id: String,
    pub denomination: Amount,
    pub origin: Origin,
}

/// Greedy split into [`DENOMINATIONS_SAT`], largest first. What is left under
/// the smallest stays out.
pub(crate) fn split(amount: Amount) -> Vec<Amount> {
    let mut rest = amount.as_sat();
    let mut out = Vec::new();
    for denomination in DENOMINATIONS_SAT {
        while rest >= denomination {
            rest -= denomination;
            out.push(Amount::from_sat(denomination));
        }
    }
    out
}

/// The items to add so the queue books the whole denominations above
/// `target`: the split of `balance - target - queued`.
pub(crate) fn grow(queued: Amount, balance: Amount, target: Amount) -> Vec<Amount> {
    split(balance.saturating_sub(target).saturating_sub(queued))
}

/// How the queue changes to fit under a cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fit {
    /// Ids of the items to drop, newest first.
    pub removed: Vec<String>,
    /// New items for the freed money that still fits under the cap.
    pub regrow: Vec<Amount>,
    /// The newest removed item's origin, which the regrown items take;
    /// `None` when nothing is removed.
    pub origin: Option<Origin>,
}

/// Shrink `booked` (oldest first) from the newest item until its total is at
/// most `cap`, then split again what the removed items freed up to the cap.
pub(crate) fn fit(booked: &[Booked], cap: Amount) -> Fit {
    let mut kept = booked.iter().map(|b| b.denomination.msat).sum::<u64>();
    let mut freed = 0u64;
    let mut removed = Vec::new();
    let mut origin = None;
    for item in booked.iter().rev() {
        if kept <= cap.msat {
            break;
        }
        kept -= item.denomination.msat;
        freed += item.denomination.msat;
        removed.push(item.id.clone());
        origin.get_or_insert(item.origin);
    }
    let room = cap.msat.saturating_sub(kept).min(freed);
    Fit {
        removed,
        regrow: split(Amount { msat: room }),
        origin,
    }
}

/// `clamp(short × (target / max(estimate, 1))², short, max)`, rounded down.
pub(crate) fn mean_wait_secs(estimate: u32, target: u32, timing: &ExitTiming) -> u64 {
    let estimate = u128::from(estimate.max(1));
    let target = u128::from(target);
    let wait = u128::from(timing.short_wait_secs) * target * target / (estimate * estimate);
    let wait = u64::try_from(wait).unwrap_or(u64::MAX);
    wait.clamp(
        timing.short_wait_secs,
        timing.max_wait_secs.max(timing.short_wait_secs),
    )
}

/// The exponential quantile `ceil(-ln(1 - uniform) × mean)`, for `uniform` in
/// `[0, 1)`. Outside it: exactly 1 gives `u64::MAX` (0 with a mean of 0), and
/// above 1 or NaN gives 0. A mean of 0 always gives 0.
pub(crate) fn draw_delay(mean_secs: u64, uniform: f64) -> u64 {
    let delay = (-(1.0 - uniform).ln() * mean_secs as f64).ceil();
    // `as` saturates, and maps NaN to 0.
    delay as u64
}

/// What `readiness` decides from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Conditions {
    pub now: Timestamp,
    /// Saving is set up, open and done with its first scan.
    pub saving_usable: bool,
    /// The federation is connected.
    pub online: bool,
    /// When it last connected.
    pub online_since: Option<Timestamp>,
    /// The current quote for the exit; `None` when there is none.
    pub fee: Option<Amount>,
    /// Spending's balance outside the queue, which pays exit fees.
    pub for_spending: Amount,
    pub settings: Settings,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Readiness {
    Waiting(QueueItemStatus),
    /// Free to leave once its drawn delay has passed.
    Ready,
}

/// The status of a queued item, first match wins: Saving not usable or the
/// money too new, then offline or unquoted, the grace period, the fee guard,
/// and a fee Spending cannot pay.
pub(crate) fn readiness(origin_at: Timestamp, denomination: Amount, c: &Conditions) -> Readiness {
    let s = &c.settings;
    if !c.saving_usable || c.now.saturating_sub(origin_at) < s.min_time_in_pool_secs {
        return Readiness::Waiting(QueueItemStatus::Queued);
    }
    let deferred = |reason| Readiness::Waiting(QueueItemStatus::Deferred { reason });
    let Some(fee) = c.fee.filter(|_| c.online) else {
        return deferred(DeferReason::Offline);
    };
    // Online without a start time cannot show the grace has passed.
    let in_grace = c
        .online_since
        .is_none_or(|since| c.now.saturating_sub(since) < s.online_grace_secs);
    if in_grace {
        return deferred(DeferReason::GracePeriod);
    }
    if u128::from(fee.msat) * 100 > u128::from(denomination.msat) * u128::from(s.fee_guard_percent)
    {
        return deferred(DeferReason::FeeGuard);
    }
    if c.for_spending < fee {
        return deferred(DeferReason::FeeUnaffordable);
    }
    Readiness::Ready
}

/// The earliest time the next automatic exit of a federation may go.
pub(crate) fn not_before(
    last_automatic_exit: Option<Timestamp>,
    timing: &ExitTiming,
) -> Option<Timestamp> {
    last_automatic_exit.map(|last| last.saturating_add(timing.spacing_secs))
}

/// `current` plus the deposits expected in `expected_secs` at `per_session`
/// deposits per session of `session_secs`, rounded to the nearest.
pub(crate) fn attainable(
    current: u32,
    per_session: f64,
    expected_secs: u64,
    session_secs: u64,
) -> u32 {
    let sessions = expected_secs as f64 / session_secs.max(1) as f64;
    let more = (per_session.max(0.0) * sessions).round();
    // `as` saturates.
    current.saturating_add(more as u32)
}

/// 0 when empty, the lower middle for an even count. Sorts `values`.
pub(crate) fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    match values.len() {
        0 => 0,
        n => values[(n - 1) / 2],
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use super::*;
    use crate::api::{DeferReason, QueueItemStatus, Settings};

    const BTC: u64 = 100_000_000;

    /// `btc` given in units of 1e-6 BTC (100 sat), so test values stay exact.
    fn ubtc(micro: u64) -> Amount {
        Amount::from_sat(micro * 100)
    }

    fn sats(amounts: &[Amount]) -> Vec<u64> {
        amounts.iter().map(|a| a.as_sat()).collect()
    }

    #[test]
    fn split_is_greedy_largest_first() {
        assert_eq!(
            sats(&split(ubtc(420_000))),
            vec![20_000_000, 20_000_000, 2_000_000]
        );
        assert_eq!(
            sats(&split(ubtc(13_700))),
            vec![1_000_000, 200_000, 100_000]
        );
        assert!(split(Amount::from_sat(99_999)).is_empty());
        assert_eq!(DENOMINATIONS_SAT[0], BTC);
        assert_eq!(DENOMINATIONS_SAT[9], 100_000);
    }

    #[test]
    fn grow_queues_only_whole_denominations_above_target() {
        assert_eq!(
            sats(&grow(Amount::ZERO, ubtc(470_000), ubtc(50_000))),
            vec![20_000_000, 20_000_000, 2_000_000]
        );
        assert!(grow(Amount::ZERO, ubtc(50_900), ubtc(50_000)).is_empty());
    }

    #[test]
    fn a_send_shrinks_the_queue_from_the_newest_item() {
        let booked = |id: &str, micro: u64, session: u64| Booked {
            id: id.to_string(),
            denomination: ubtc(micro),
            origin: Origin {
                session,
                at: session * 10,
            },
        };
        let queue = [
            booked("a", 200_000, 1),
            booked("b", 200_000, 2),
            booked("c", 20_000, 3),
        ];
        let fitted = fit(&queue, Amount::from_sat(38_999_700));
        assert_eq!(fitted.removed, vec!["c".to_string(), "b".to_string()]);
        assert_eq!(fitted.regrow, split(Amount::from_sat(18_999_700)));
        assert_eq!(
            sats(&fitted.regrow),
            vec![
                10_000_000, 5_000_000, 2_000_000, 1_000_000, 500_000, 200_000, 200_000
            ]
        );
        assert_eq!(fitted.origin, Some(queue[2].origin));
    }

    #[test]
    fn a_queue_within_the_cap_is_left_alone() {
        let queue = [Booked {
            id: "a".to_string(),
            denomination: ubtc(20_000),
            origin: Origin { session: 1, at: 1 },
        }];
        let fitted = fit(&queue, ubtc(20_000));
        assert!(fitted.removed.is_empty());
        assert!(fitted.regrow.is_empty());
        assert_eq!(fitted.origin, None);
    }

    #[test]
    fn mean_wait_is_short_at_the_target() {
        assert_eq!(mean_wait_secs(50, 50, &ExitTiming::DEFAULT), 10_800);
    }

    #[test]
    fn mean_wait_quadruples_at_half_the_target() {
        assert_eq!(mean_wait_secs(25, 50, &ExitTiming::DEFAULT), 43_200);
    }

    #[test]
    fn mean_wait_is_capped_with_no_deposits() {
        assert_eq!(mean_wait_secs(0, 50, &ExitTiming::DEFAULT), 2_592_000);
    }

    #[test]
    fn mean_wait_does_not_shrink_past_the_target() {
        assert_eq!(mean_wait_secs(500, 50, &ExitTiming::DEFAULT), 10_800);
    }

    #[test]
    fn delay_is_the_exponential_quantile() {
        assert_eq!(draw_delay(10_800, 0.0), 0);
        assert_eq!(draw_delay(10_800, 0.5), 7_486);
    }

    #[test]
    fn delay_near_one_stays_finite() {
        assert_eq!(draw_delay(10_800, 1.0 - 2f64.powi(-20)), 149_720);
        // The largest `f64` below 1, the most `gen::<f64>()` can return.
        assert_eq!(draw_delay(10_800, 1.0 - f64::EPSILON / 2.0), 396_758);
        assert_eq!(draw_delay(10_800, 1.0), u64::MAX);
        assert_eq!(draw_delay(10_800, 1.5), 0);
    }

    #[test]
    fn a_mean_of_zero_draws_no_delay() {
        assert_eq!(draw_delay(0, 0.0), 0);
        assert_eq!(draw_delay(0, 0.5), 0);
        assert_eq!(draw_delay(0, 1.0 - f64::EPSILON / 2.0), 0);
        assert_eq!(draw_delay(0, 1.0), 0);
    }

    #[test]
    fn delays_average_their_mean() {
        let mut rng = StdRng::seed_from_u64(7);
        let draws = 10_000;
        let total: u64 = (0..draws)
            .map(|_| draw_delay(10_800, rng.r#gen::<f64>()))
            .sum();
        let average = total as f64 / f64::from(draws);
        assert!(
            (average - 10_800.0).abs() <= 10_800.0 * 0.02,
            "average {average}"
        );
    }

    #[test]
    fn readiness_follows_the_order() {
        let now = 1_000_000;
        let denomination = Amount::from_sat(100_000);
        let base = Conditions {
            now,
            saving_usable: true,
            online: true,
            online_since: Some(now - 600),
            fee: Some(Amount::from_sat(1_000)),
            for_spending: Amount::from_sat(10_000),
            settings: Settings::default(),
        };
        let old = now - 86_400;
        let check = |origin_at, c: &Conditions| readiness(origin_at, denomination, c);
        let deferred = |reason| Readiness::Waiting(QueueItemStatus::Deferred { reason });

        let no_saving = Conditions {
            saving_usable: false,
            ..base.clone()
        };
        assert_eq!(
            check(old, &no_saving),
            Readiness::Waiting(QueueItemStatus::Queued)
        );
        assert_eq!(
            check(now - 86_399, &base),
            Readiness::Waiting(QueueItemStatus::Queued)
        );
        let offline = Conditions {
            online: false,
            online_since: None,
            ..base.clone()
        };
        assert_eq!(check(old, &offline), deferred(DeferReason::Offline));
        let no_quote = Conditions {
            fee: None,
            ..base.clone()
        };
        assert_eq!(check(old, &no_quote), deferred(DeferReason::Offline));
        let fresh = Conditions {
            online_since: Some(now - 599),
            ..base.clone()
        };
        assert_eq!(check(old, &fresh), deferred(DeferReason::GracePeriod));
        let dear = Conditions {
            fee: Some(Amount::from_sat(2_001)),
            ..base.clone()
        };
        assert_eq!(check(old, &dear), deferred(DeferReason::FeeGuard));
        let at_guard = Conditions {
            fee: Some(Amount::from_sat(2_000)),
            ..base.clone()
        };
        assert_eq!(check(old, &at_guard), Readiness::Ready);
        let unstarted = Conditions {
            online_since: None,
            ..base.clone()
        };
        assert_eq!(check(old, &unstarted), deferred(DeferReason::GracePeriod));
        let poor = Conditions {
            for_spending: Amount::from_sat(1_000),
            fee: Some(Amount::from_sat(1_500)),
            ..base.clone()
        };
        assert_eq!(check(old, &poor), deferred(DeferReason::FeeUnaffordable));
        assert_eq!(check(old, &base), Readiness::Ready);
    }

    #[test]
    fn automatic_exits_are_an_hour_apart() {
        assert_eq!(not_before(Some(1_000), &ExitTiming::DEFAULT), Some(4_600));
        assert_eq!(not_before(None, &ExitTiming::DEFAULT), None);
    }

    #[test]
    fn attainable_adds_the_deposits_expected_meanwhile() {
        assert_eq!(attainable(10, 0.2, 3_600, 180), 14);
        // 0.25 × 2 sessions = 0.5 more, rounded half away from zero.
        assert_eq!(attainable(10, 0.25, 360, 180), 11);
    }

    #[test]
    fn median_takes_the_lower_middle() {
        assert_eq!(median(&mut []), 0);
        assert_eq!(median(&mut [5, 1, 3]), 3);
        assert_eq!(median(&mut [4, 1, 3, 2]), 2);
    }
}
