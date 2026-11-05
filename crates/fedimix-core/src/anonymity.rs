//! Counting the anonymity set from a federation's signed session history:
//! the deposits that could plausibly have funded an exit. Pure: the caller
//! passes the synced deposits and the wallet's own.

use std::collections::HashSet;

use crate::api::Amount;

/// How far back the per-session deposit rate looks.
pub(crate) const FEDERATION_WINDOW_SESSIONS: u64 = 480;
/// The session length assumed until one has been measured.
pub(crate) const DEFAULT_SESSION_SECS: u64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepositSource {
    OnChain,
    Lightning,
}

/// One countable deposit in a federation session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionDeposit {
    pub source: DepositSource,
    /// On chain the gross value, for Lightning the contract amount.
    pub amount: Amount,
    /// `"txid:vout"` on chain.
    pub reference: Option<String>,
}

/// This wallet's own deposits, which never count.
#[derive(Debug, Default)]
pub(crate) struct Own {
    pub outpoints: HashSet<String>,
    pub txids: HashSet<String>,
    /// `(session, amount)` of each Lightning receive; these are subtracted by
    /// count, as a session's receives carry no reference.
    pub lightning: Vec<(u64, Amount)>,
}

impl Own {
    fn has_on_chain(&self, deposit: &SessionDeposit) -> bool {
        deposit.reference.as_deref().is_some_and(|outpoint| {
            self.outpoints.contains(outpoint)
                || outpoint
                    .split_once(':')
                    .is_some_and(|(txid, _)| self.txids.contains(txid))
        })
    }
}

/// Deposits of at least `denomination` in sessions within `sessions`, own
/// on-chain ones left out and own Lightning ones subtracted, never below 0.
fn count(
    deposits: &[(u64, SessionDeposit)],
    sessions: impl Fn(u64) -> bool,
    denomination: Amount,
    own: &Own,
) -> u64 {
    let counts = |session: u64, amount: Amount| sessions(session) && amount >= denomination;
    let mut on_chain = 0u64;
    let mut lightning = 0u64;
    for (session, deposit) in deposits {
        if !counts(*session, deposit.amount) {
            continue;
        }
        match deposit.source {
            DepositSource::OnChain if !own.has_on_chain(deposit) => on_chain += 1,
            DepositSource::OnChain => {}
            DepositSource::Lightning => lightning += 1,
        }
    }
    let own_lightning = own
        .lightning
        .iter()
        .filter(|(session, amount)| counts(*session, *amount))
        .count() as u64;
    on_chain + lightning.saturating_sub(own_lightning)
}

/// The anonymity estimate of `denomination`: other people's deposits of at
/// least that much in sessions strictly after `after`.
pub(crate) fn estimate(
    deposits: &[(u64, SessionDeposit)],
    after: u64,
    denomination: Amount,
    own: &Own,
) -> u32 {
    let n = count(deposits, |s| s > after, denomination, own);
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Other people's deposits of at least `denomination` per session, over the
/// newest `min(480, synced)` sessions ending at `head`; 0 with none synced.
pub(crate) fn per_session(
    deposits: &[(u64, SessionDeposit)],
    head: u64,
    synced: u64,
    denomination: Amount,
    own: &Own,
) -> f64 {
    let window = synced
        .min(FEDERATION_WINDOW_SESSIONS)
        .min(head.saturating_add(1));
    if window == 0 {
        return 0.0;
    }
    let first = head + 1 - window;
    let n = count(deposits, |s| (first..=head).contains(&s), denomination, own);
    n as f64 / window as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ubtc(micro: u64) -> Amount {
        Amount::from_sat(micro * 100)
    }

    fn on_chain(micro: u64, txid: &str) -> SessionDeposit {
        SessionDeposit {
            source: DepositSource::OnChain,
            amount: ubtc(micro),
            reference: Some(format!("{txid}:0")),
        }
    }

    /// Sessions 10-14: 0.0005, 0.001, 0.01 and 0.02 BTC on chain, then a
    /// 0.01 BTC Lightning receive.
    fn fixture() -> Vec<(u64, SessionDeposit)> {
        vec![
            (10, on_chain(500, "aa")),
            (11, on_chain(1_000, "bb")),
            (12, on_chain(10_000, "cc")),
            (13, on_chain(20_000, "dd")),
            (
                14,
                SessionDeposit {
                    source: DepositSource::Lightning,
                    amount: ubtc(10_000),
                    reference: None,
                },
            ),
        ]
    }

    #[test]
    fn counts_deposits_at_or_above_the_denomination() {
        assert_eq!(estimate(&fixture(), 9, ubtc(10_000), &Own::default()), 3);
        assert_eq!(estimate(&fixture(), 9, ubtc(1_000), &Own::default()), 4);
    }

    #[test]
    fn the_origin_session_is_not_counted() {
        assert_eq!(estimate(&fixture(), 12, ubtc(10_000), &Own::default()), 2);
    }

    #[test]
    fn own_deposits_do_not_count() {
        let d = ubtc(10_000);
        let by_outpoint = Own {
            outpoints: ["dd:0".to_string()].into(),
            ..Own::default()
        };
        assert_eq!(estimate(&fixture(), 9, d, &by_outpoint), 2);
        let by_txid = Own {
            txids: ["cc".to_string()].into(),
            ..Own::default()
        };
        assert_eq!(estimate(&fixture(), 9, d, &by_txid), 2);
        let by_lightning = Own {
            lightning: vec![(14, d)],
            ..Own::default()
        };
        assert_eq!(estimate(&fixture(), 9, d, &by_lightning), 2);
        let everything = Own {
            outpoints: ["dd:0".to_string()].into(),
            txids: ["cc".to_string()].into(),
            lightning: vec![(14, d), (14, d), (13, d)],
        };
        assert_eq!(estimate(&fixture(), 9, d, &everything), 0);
    }

    #[test]
    fn per_session_uses_the_recent_window() {
        let deposits: Vec<_> = (0..600).map(|s| (s, on_chain(1_000, "ee"))).collect();
        let rate = per_session(&deposits, 599, 600, ubtc(1_000), &Own::default());
        assert!((rate - 1.0).abs() < 1e-9, "rate {rate}");
        // Only the newest 480 sessions count: one deposit per four sessions in
        // them, and every older session full.
        let sparse: Vec<_> = (0..600)
            .filter(|s| *s < 120 || s % 4 == 0)
            .map(|s| (s, on_chain(1_000, "ee")))
            .collect();
        let rate = per_session(&sparse, 599, 600, ubtc(1_000), &Own::default());
        assert!((rate - 0.25).abs() < 1e-9, "rate {rate}");
        // Fewer synced sessions than the window: the rate is over those.
        let rate = per_session(&deposits[590..], 599, 10, ubtc(1_000), &Own::default());
        assert!((rate - 1.0).abs() < 1e-9, "rate {rate}");
        assert_eq!(per_session(&[], 599, 0, ubtc(1_000), &Own::default()), 0.0);
    }
}
