//! Parsing of user-entered destinations and BTC amounts.

use crate::api::Amount;
use crate::error::{FedimixError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationKind {
    Lightning,
    OnChain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub kind: DestinationKind,
    pub normalized: String,
    pub amount: Option<Amount>,
}

const NOT_A_DESTINATION: &str = "This is not a valid Lightning invoice or bitcoin address";
const SATS_PER_BTC: u64 = 100_000_000;
const MAX_SUPPLY_SAT: u64 = 21_000_000 * SATS_PER_BTC;
const MSAT_PER_BTC: u64 = 100_000_000_000;
/// Longest prefixes first: `lnbc` would also match `lnbcrt`.
const INVOICE_PREFIXES: [&str; 4] = ["lnbcrt", "lnbc", "lntbs", "lntb"];
const ADDRESS_PREFIXES: [&str; 3] = ["bcrt1", "bc1", "tb1"];

fn invalid_amount(why: &str) -> FedimixError {
    FedimixError::InvalidAmount(why.to_string())
}

fn invalid_destination() -> FedimixError {
    FedimixError::InvalidDestination(NOT_A_DESTINATION.to_string())
}

/// Parse a decimal BTC amount with at most 8 decimal places.
pub fn parse_btc(s: &str) -> Result<Amount> {
    let s = s.trim();
    if s.is_empty() {
        return Err(invalid_amount("Enter an amount"));
    }
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if (whole.is_empty() && frac.is_empty())
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid_amount("Enter a valid amount"));
    }
    if frac.len() > 8 {
        return Err(invalid_amount("At most 8 decimal places"));
    }
    let whole_n: u64 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| invalid_amount("Amount too large"))?
    };
    let frac_n: u64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<8}")
            .parse()
            .map_err(|_| invalid_amount("Enter a valid amount"))?
    };
    let sat = whole_n
        .checked_mul(SATS_PER_BTC)
        .and_then(|w| w.checked_add(frac_n))
        .filter(|&v| v <= MAX_SUPPLY_SAT)
        .ok_or_else(|| invalid_amount("Amount too large"))?;
    Ok(Amount::from_sat(sat))
}

/// Bech32 is all lower or all upper case; returns the lower-case form.
fn lower_bech32(s: &str) -> Result<String> {
    let lower = s.to_ascii_lowercase();
    let upper = s.to_ascii_uppercase();
    if s != lower && s != upper {
        return Err(invalid_destination());
    }
    Ok(lower)
}

fn strip_scheme<'a>(input: &'a str, scheme: &str) -> Option<&'a str> {
    let head = input.get(..scheme.len())?;
    head.eq_ignore_ascii_case(scheme)
        .then(|| &input[scheme.len()..])
}

/// Amount in the invoice's human-readable part, e.g. `2500u`.
fn invoice_hrp_amount(hrp_amount: &str) -> Result<Option<Amount>> {
    if hrp_amount.is_empty() {
        return Ok(None);
    }
    let digits_end = hrp_amount
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(hrp_amount.len());
    let (digits, multiplier) = hrp_amount.split_at(digits_end);
    let value: u64 = digits.parse().map_err(|_| invalid_destination())?;
    let msat = match multiplier {
        "" => value.checked_mul(MSAT_PER_BTC),
        "m" => value.checked_mul(MSAT_PER_BTC / 1_000),
        "u" => value.checked_mul(MSAT_PER_BTC / 1_000_000),
        "n" => value.checked_mul(MSAT_PER_BTC / 1_000_000_000),
        // A pico-bitcoin is a tenth of a millisatoshi.
        "p" if value.is_multiple_of(10) => Some(value / 10),
        _ => None,
    };
    msat.filter(|&m| m / 1000 <= MAX_SUPPLY_SAT)
        .map(|msat| Some(Amount { msat }))
        .ok_or_else(invalid_destination)
}

fn parse_invoice(body: &str) -> Result<Destination> {
    let normalized = lower_bech32(body)?;
    let prefix = INVOICE_PREFIXES
        .iter()
        .find(|p| normalized.starts_with(**p))
        .ok_or_else(invalid_destination)?;
    // The data part cannot contain `1`, so the last one is the separator.
    let separator = normalized.rfind('1').ok_or_else(invalid_destination)?;
    let data = &normalized[separator + 1..];
    if separator < prefix.len() || data.is_empty() || !is_alphanumeric(data) {
        return Err(invalid_destination());
    }
    let amount = invoice_hrp_amount(&normalized[prefix.len()..separator])?;
    Ok(Destination {
        kind: DestinationKind::Lightning,
        normalized,
        amount,
    })
}

fn is_alphanumeric(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn parse_address(body: &str) -> Result<Destination> {
    let normalized = lower_bech32(body)?;
    let valid = ADDRESS_PREFIXES.iter().any(|p| {
        normalized
            .strip_prefix(p)
            .is_some_and(|rest| !rest.is_empty() && is_alphanumeric(rest))
    });
    if !valid {
        return Err(invalid_destination());
    }
    Ok(Destination {
        kind: DestinationKind::OnChain,
        normalized,
        amount: None,
    })
}

/// BIP21 `bitcoin:` body: address, then optional `?key=value&...`.
fn parse_bip21(body: &str) -> Result<Destination> {
    let (address, query) = body.split_once('?').unwrap_or((body, ""));
    let mut destination = parse_address(address)?;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = key.to_ascii_lowercase();
        if key.starts_with("req-") {
            // BIP21: unknown required parameters make the URI unusable.
            return Err(invalid_destination());
        }
        if key == "amount" {
            let amount = parse_btc(value).map_err(|_| invalid_destination())?;
            destination.amount = (amount != Amount::ZERO).then_some(amount);
        }
    }
    Ok(destination)
}

/// Classify and normalize what the user entered as a recipient.
///
/// Accepts Lightning invoices (bare or `lightning:`), bech32 addresses (bare
/// or BIP21 `bitcoin:`) of mainnet, testnet and regtest, in either case. The
/// amount is the one the destination itself carries. Checksums are not
/// verified yet.
pub fn parse_destination(input: &str) -> Result<Destination> {
    let input = input.trim();
    if let Some(body) = strip_scheme(input, "lightning:") {
        return parse_invoice(body);
    }
    if let Some(body) = strip_scheme(input, "bitcoin:") {
        return parse_bip21(body);
    }
    if input.to_ascii_lowercase().starts_with("ln") {
        parse_invoice(input)
    } else {
        parse_address(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DestinationKind::{Lightning, OnChain};

    fn sat(n: u64) -> Amount {
        Amount::from_sat(n)
    }

    fn parsed(s: &str) -> (DestinationKind, String, Option<Amount>) {
        let d = parse_destination(s).unwrap();
        (d.kind, d.normalized, d.amount)
    }

    #[test]
    fn parse_btc_rejects_bad_input() {
        for bad in ["", "  ", "abc", "-1", "0.123456789", "21000001", "."] {
            assert!(
                matches!(parse_btc(bad), Err(FedimixError::InvalidAmount(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn parse_btc_accepts() {
        assert_eq!(parse_btc("0.0012"), Ok(sat(120_000)));
        assert_eq!(parse_btc(" 1 "), Ok(sat(100_000_000)));
        assert_eq!(parse_btc("21000000"), Ok(sat(2_100_000_000_000_000)));
    }

    #[test]
    fn plain_addresses_on_every_network() {
        for addr in ["bc1qabc", "tb1qabc", "bcrt1qabc"] {
            assert_eq!(parsed(addr), (OnChain, addr.to_string(), None));
        }
    }

    #[test]
    fn invoices_on_every_network() {
        for inv in ["lnbc1x", "lntb1x", "lntbs1x", "lnbcrt1x"] {
            assert_eq!(parsed(inv), (Lightning, inv.to_string(), None));
        }
    }

    #[test]
    fn invoice_amount_comes_from_the_prefix() {
        assert_eq!(parsed("lnbc1200u1abc").2, Some(sat(120_000)));
        assert_eq!(parsed("lnbc25m1abc").2, Some(sat(2_500_000)));
        assert_eq!(parsed("lnbcrt500n1abc").2, Some(sat(50)));
        assert_eq!(parsed("lnbc2500p1abc").2, Some(Amount { msat: 250 }));
        assert_eq!(parsed("lntb2n1abc").2, Some(Amount { msat: 200 }));
    }

    #[test]
    fn invoice_with_unusable_amount_is_rejected() {
        // Sub-millisatoshi, unknown multiplier, overflow, no separator.
        for bad in [
            "lnbc15p1abc",
            "lnbc5x1abc",
            "lnbc99999999999m1abc",
            "lnbcabc",
        ] {
            assert!(parse_destination(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn uppercase_is_normalized() {
        assert_eq!(parsed("BC1QABC"), (OnChain, "bc1qabc".to_string(), None));
        assert_eq!(parsed("LNBC1X"), (Lightning, "lnbc1x".to_string(), None));
    }

    #[test]
    fn mixed_case_is_rejected() {
        assert!(parse_destination("bC1qabc").is_err());
        assert!(parse_destination("LNbc1x").is_err());
    }

    #[test]
    fn surrounding_space_is_ignored() {
        assert_eq!(parsed("  bc1qabc\n").1, "bc1qabc");
    }

    #[test]
    fn lightning_scheme_is_stripped() {
        assert_eq!(parsed("lightning:lnbc1x").1, "lnbc1x");
        assert_eq!(
            parsed("LIGHTNING:LNBC1200U1X"),
            (Lightning, "lnbc1200u1x".to_string(), Some(sat(120_000)))
        );
        assert!(parse_destination("lightning:bc1qabc").is_err());
    }

    #[test]
    fn bip21_with_amount() {
        assert_eq!(
            parsed("bitcoin:bc1qabc?amount=0.001&label=shop"),
            (OnChain, "bc1qabc".to_string(), Some(sat(100_000)))
        );
        assert_eq!(
            parsed("BITCOIN:BCRT1QABC?AMOUNT=0.5"),
            (OnChain, "bcrt1qabc".to_string(), Some(sat(50_000_000)))
        );
        assert_eq!(parsed("bitcoin:tb1qabc").2, None);
        assert_eq!(parsed("bitcoin:tb1qabc?label=x").2, None);
    }

    #[test]
    fn bip21_rejects_what_it_cannot_honour() {
        for bad in [
            "bitcoin:bc1qabc?amount=lots",
            "bitcoin:bc1qabc?req-pay=1",
            "bitcoin:lnbc1x",
            "bitcoin:",
        ] {
            assert!(parse_destination(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn garbage_is_invalid_destination() {
        for bad in ["", "   ", "garbage", "bc1", "lnbc", "bc1q abc", "http://x"] {
            assert!(
                matches!(
                    parse_destination(bad),
                    Err(FedimixError::InvalidDestination(_))
                ),
                "{bad:?}"
            );
        }
    }
}
