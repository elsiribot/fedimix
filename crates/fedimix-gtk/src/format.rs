//! Display formatting for amounts, dates and durations.

use fedimix_core::api::{Amount, Timestamp};

const SATS_PER_BTC: u64 = 100_000_000;

/// Truncated 4-decimal number, or the dust marker.
fn plain4(a: Amount) -> String {
    if a.msat == 0 {
        return "0".to_string();
    }
    let units = a.as_sat() / 10_000;
    if units == 0 {
        return "<0.0001".to_string();
    }
    format!("{}.{:04}", units / 10_000, units % 10_000)
}

pub fn btc_plain(a: Amount) -> String {
    plain4(a)
}

pub fn btc(a: Amount) -> String {
    format!("{} BTC", plain4(a))
}

pub fn btc_short(a: Amount) -> String {
    let s = plain4(a);
    if s.contains('.') && !s.starts_with('<') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

pub fn btc_exact(a: Amount) -> String {
    let sat = a.as_sat();
    format!("{}.{:08} BTC", sat / SATS_PER_BTC, sat % SATS_PER_BTC)
}

pub fn signed(a: Amount, incoming: bool) -> String {
    let sign = if incoming { '+' } else { '\u{2212}' };
    format!("{sign}{}", plain4(a))
}

/// Rounds up: fees are the main use, and a fee never reads as "0 sat".
pub fn sats(a: Amount) -> String {
    format!("{} sat", a.as_sat_ceil())
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// (year, month 1..=12, day 1..=31) for days since the Unix epoch.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Pure variant of [`date`] with an explicit UTC offset in seconds.
pub fn date_with_offset(ts: Timestamp, now: Timestamp, offset_secs: i64) -> String {
    let local = ts as i64 + offset_secs;
    let local_now = now as i64 + offset_secs;
    let day = local.div_euclid(86_400);
    if day == local_now.div_euclid(86_400) {
        let secs = local.rem_euclid(86_400);
        format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
    } else {
        let (_, m, d) = civil_from_days(day);
        format!("{} {}", MONTHS[(m - 1) as usize], d)
    }
}

fn local_offset_secs(ts: Timestamp) -> i64 {
    gtk::glib::DateTime::from_unix_local(ts as i64)
        .map(|dt| dt.utc_offset().as_seconds())
        .unwrap_or(0)
}

/// Same local day as `now`: "10:42", otherwise "Oct 29".
pub fn date(ts: Timestamp, now: Timestamp) -> String {
    date_with_offset(ts, now, local_offset_secs(ts))
}

pub fn duration_rough(secs: u64) -> String {
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("~1 {unit}")
        } else {
            format!("~{n} {unit}s")
        }
    };
    if secs >= 86_400 {
        plural(secs / 86_400, "day")
    } else if secs >= 3600 {
        plural(secs / 3600, "hour")
    } else {
        format!("~{} min", (secs / 60).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sat(n: u64) -> Amount {
        Amount::from_sat(n)
    }

    #[test]
    fn btc_formats_mockup_values() {
        assert_eq!(btc(sat(120_340_000)), "1.2034 BTC");
        assert_eq!(btc_plain(sat(120_340_000)), "1.2034");
        assert_eq!(btc_exact(sat(12_345)), "0.00012345 BTC");
        assert_eq!(sats(sat(420)), "420 sat");
    }

    #[test]
    fn btc_never_shows_zero_for_dust() {
        assert_eq!(btc(sat(1)), "<0.0001 BTC");
        assert_eq!(btc(Amount::ZERO), "0 BTC");
        assert_eq!(btc_plain(Amount::ZERO), "0");
    }

    #[test]
    fn btc_short_trims() {
        assert_eq!(btc_short(sat(5_000_000)), "0.05");
        assert_eq!(btc_short(sat(42_000_000)), "0.42");
        assert_eq!(btc_short(sat(100_000_000)), "1");
        assert_eq!(btc_short(sat(1_120_000)), "0.0112");
    }

    #[test]
    fn signed_uses_true_minus() {
        assert_eq!(signed(sat(10_000_000), true), "+0.1000");
        assert_eq!(signed(sat(25_000_000), false), "\u{2212}0.2500");
    }

    #[test]
    fn sats_round_up() {
        assert_eq!(sats(Amount { msat: 1 }), "1 sat");
        assert_eq!(sats(Amount { msat: 1500 }), "2 sat");
    }

    #[test]
    fn date_same_day_and_other_day() {
        // 2026-10-30 10:42:00 UTC
        let now = 1_793_356_920;
        assert_eq!(date_with_offset(now, now, 0), "10:42");
        assert_eq!(date_with_offset(now - 86_400, now, 0), "Oct 29");
        // a +2h offset moves 23:42 UTC of the previous day into today
        assert_eq!(date_with_offset(now - 11 * 3600, now, 0), "Oct 29");
        assert_eq!(date_with_offset(now - 11 * 3600, now, 3600 * 2), "01:42");
        assert_eq!(date_with_offset(now - 9 * 3600, now, 0), "01:42");
    }

    #[test]
    fn duration_rough_units() {
        assert_eq!(duration_rough(86_400), "~1 day");
        assert_eq!(duration_rough(3 * 3600), "~3 hours");
        assert_eq!(duration_rough(1200), "~20 min");
        assert_eq!(duration_rough(3600), "~1 hour");
        assert_eq!(duration_rough(5), "~1 min");
    }
}
