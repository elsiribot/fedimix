//! Presentation logic: turns core state into display-ready view structs.

use crate::format;
use fedimix_core::api::*;

const DOT: &str = "\u{b7}";
const CHEVRON: &str = "\u{203a}";

#[derive(Debug, Clone, PartialEq)]
pub struct Caption {
    pub text: String,
    pub opens_queue: bool,
    pub shows_refill: bool,
    /// Bar fill, always within 0..=1.
    pub fraction: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagView {
    pub text: String,
    pub css_class: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRowView {
    pub icon: &'static str,
    pub title: &'static str,
    pub tag: Option<TagView>,
    pub amount: String,
    pub trailing: String,
    pub nested: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueRowView {
    pub label: String,
    pub fraction: f64,
    pub detail: String,
}

/// `num / den` clamped to 0..=1; a zero denominator yields 1 when there is
/// anything in the numerator, else 0 (never NaN).
fn fraction(num: u64, den: u64) -> f64 {
    if den == 0 {
        return if num == 0 { 0.0 } else { 1.0 };
    }
    (num as f64 / den as f64).clamp(0.0, 1.0)
}

pub fn spending_caption(s: &SpendingState) -> Caption {
    let spend = format::btc_short(s.for_spending);
    if s.for_spending < s.target {
        return Caption {
            text: format!("{spend} of {} target", format::btc_short(s.target)),
            opens_queue: false,
            shows_refill: true,
            fraction: fraction(s.for_spending.msat, s.target.msat),
        };
    }
    if s.moving == Amount::ZERO {
        return Caption {
            text: format!("{spend} for spending"),
            opens_queue: false,
            shows_refill: false,
            fraction: 1.0,
        };
    }
    let moving = format::btc_short(s.moving);
    let total = s.for_spending.msat.saturating_add(s.moving.msat);
    let (tail, opens_queue) = if s.saving_ready {
        (format!("{moving} moving to Saving {CHEVRON}"), true)
    } else {
        (format!("{moving} waiting for Saving"), false)
    };
    Caption {
        text: format!("{spend} for spending {DOT} {tail}"),
        opens_queue,
        shows_refill: false,
        fraction: fraction(s.for_spending.msat, total),
    }
}

pub fn saving_banner(s: &SpendingState) -> Option<String> {
    (!s.saving_ready && s.moving > Amount::ZERO).then(|| {
        format!(
            "Set up Saving to move {} BTC to cold storage",
            format::btc_short(s.moving)
        )
    })
}

pub fn in_flight_line(s: &SavingState) -> Option<String> {
    (s.in_flight > Amount::ZERO)
        .then(|| format!("+{} depositing {CHEVRON}", format::btc(s.in_flight)))
}

pub fn privacy_tag(t: &PrivacyTag) -> TagView {
    match t {
        PrivacyTag::Anonymity { estimate } => TagView {
            text: format!("anon set {estimate}"),
            css_class: "tag-mixed",
        },
        PrivacyTag::EarlyExit { estimate } => TagView {
            text: format!("anon set {estimate} {DOT} early exit"),
            css_class: "tag-early",
        },
        PrivacyTag::Unmixed => TagView {
            text: "unmixed".to_string(),
            css_class: "tag-unmixed",
        },
        PrivacyTag::SmallChange => TagView {
            text: "small change".to_string(),
            css_class: "tag-change",
        },
    }
}

pub fn history_row(item: &HistoryItem, now: Timestamp) -> HistoryRowView {
    let denom_tag = |d: &Amount| TagView {
        text: format::btc_short(*d),
        css_class: "tag-mixed",
    };
    let (icon, title, tag, nested) = match &item.kind {
        HistoryKind::FromSpending { denomination } => (
            "\u{2199}",
            "From Spending",
            Some(denom_tag(denomination)),
            None,
        ),
        HistoryKind::ToSaving { denomination } => {
            ("\u{2197}", "To Saving", Some(denom_tag(denomination)), None)
        }
        HistoryKind::Sent { change } => {
            let nested = change.as_ref().map(|c| {
                let label = if c.to_spending {
                    "change \u{2192} Spending"
                } else {
                    "small change"
                };
                format!("\u{21ba} {label} {DOT} {}", format::btc_short(c.amount))
            });
            ("\u{2197}", "Sent", None, nested)
        }
        HistoryKind::Received { unmixed } => (
            "\u{2198}",
            "Received",
            unmixed.then(|| privacy_tag(&PrivacyTag::Unmixed)),
            None,
        ),
        HistoryKind::Paid => ("\u{2197}", "Paid", None, None),
        HistoryKind::ReceivedLightning => ("\u{2198}", "Received (Lightning)", None, None),
        HistoryKind::Deposit => ("\u{2198}", "Deposit", None, None),
        HistoryKind::Cover => ("\u{21c4}", "Cover traffic", None, None),
    };
    let trailing = match &item.status {
        HistoryStatus::Confirming {
            confirmations,
            required,
        } => format!("{confirmations}/{required} conf"),
        HistoryStatus::Complete => format::date(item.timestamp, now),
    };
    HistoryRowView {
        icon,
        title,
        tag,
        amount: format::signed(item.amount, item.incoming),
        trailing,
        nested,
    }
}

pub fn queue_row(item: &QueueItem, anonymity_target: u32) -> QueueRowView {
    let cur = item.anonymity_current;
    let detail = match &item.status {
        QueueItemStatus::Queued => format!("{cur} \u{2192} ~{}", item.anonymity_attainable),
        QueueItemStatus::Eligible => format!("{cur} {DOT} eligible"),
        QueueItemStatus::Scheduled { .. } => format!("{cur} {DOT} scheduled"),
        QueueItemStatus::Withdrawing { .. } => format!("{cur} {DOT} withdrawing"),
        QueueItemStatus::Landed => format!("{cur} {DOT} landed"),
    };
    QueueRowView {
        label: format::btc_short(item.denomination),
        fraction: fraction(cur.into(), anonymity_target.into()),
        detail,
    }
}

pub fn queue_summary(q: &QueueState) -> String {
    format!(
        "Est. anonymity ~{} {DOT} typical exit within {}",
        q.anonymity_estimate,
        format::duration_rough(q.typical_exit_secs)
    )
}

pub fn visible_history(items: &[HistoryItem], show_cover: bool) -> Vec<HistoryItem> {
    items
        .iter()
        .filter(|i| show_cover || i.kind != HistoryKind::Cover)
        .cloned()
        .collect()
}

pub fn send_validation(dest: &str, amount: &str, requires_amount: bool) -> Result<(), String> {
    if dest.trim().is_empty() {
        return Err("Enter a destination".to_string());
    }
    if requires_amount && format::parse_btc(amount)? == Amount::ZERO {
        return Err("Amount must be greater than zero".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sat(n: u64) -> Amount {
        Amount::from_sat(n)
    }
    fn btc(x: f64) -> Amount {
        sat((x * 1e8).round() as u64)
    }

    fn spending(for_spending: f64, moving: f64, target: f64, ready: bool) -> SpendingState {
        SpendingState {
            federation: None,
            balance: btc(for_spending + moving),
            target: btc(target),
            for_spending: btc(for_spending),
            moving: btc(moving),
            saving_ready: ready,
            notes: vec![],
            history: vec![],
        }
    }

    fn hist(kind: HistoryKind, incoming: bool, status: HistoryStatus) -> HistoryItem {
        HistoryItem {
            id: "h".into(),
            kind,
            amount: btc(0.1),
            incoming,
            status,
            timestamp: 1_793_356_920,
        }
    }

    fn qitem(status: QueueItemStatus, cur: u32, att: u32) -> QueueItem {
        QueueItem {
            id: "q".into(),
            denomination: btc(0.1),
            status,
            anonymity_current: cur,
            anonymity_attainable: att,
        }
    }

    #[test]
    fn caption_above_target() {
        let c = spending_caption(&spending(0.05, 0.42, 0.05, true));
        assert_eq!(
            c.text,
            "0.05 for spending \u{b7} 0.42 moving to Saving \u{203a}"
        );
        assert!(c.opens_queue && !c.shows_refill);
        assert!((c.fraction - 0.106).abs() < 0.001);
    }

    #[test]
    fn caption_below_target() {
        let c = spending_caption(&spending(0.0112, 0.0, 0.05, true));
        assert_eq!(c.text, "0.0112 of 0.05 target");
        assert!(c.shows_refill && !c.opens_queue);
        assert!((c.fraction - 0.224).abs() < 0.001);
    }

    #[test]
    fn caption_waiting_for_saving() {
        let c = spending_caption(&spending(0.05, 0.42, 0.05, false));
        assert_eq!(c.text, "0.05 for spending \u{b7} 0.42 waiting for Saving");
        assert!(!c.opens_queue);
    }

    #[test]
    fn caption_zero_target_no_nan() {
        for s in [
            spending(0.0, 0.0, 0.0, true),
            spending(0.0, 0.3, 0.0, true),
            spending(0.2, 0.0, 0.0, true),
        ] {
            let c = spending_caption(&s);
            assert!((0.0..=1.0).contains(&c.fraction), "{}", c.fraction);
        }
    }

    #[test]
    fn banner_only_when_no_saving_and_moving() {
        assert_eq!(
            saving_banner(&spending(0.05, 0.42, 0.05, false)),
            Some("Set up Saving to move 0.42 BTC to cold storage".to_string())
        );
        assert_eq!(saving_banner(&spending(0.05, 0.42, 0.05, true)), None);
        assert_eq!(saving_banner(&spending(0.05, 0.0, 0.05, false)), None);
    }

    #[test]
    fn in_flight_hidden_when_zero() {
        let mut s = SavingState {
            device: None,
            balance: Amount::ZERO,
            in_flight: btc(0.42),
            coins: vec![],
            history: vec![],
        };
        assert_eq!(
            in_flight_line(&s),
            Some("+0.4200 BTC depositing \u{203a}".to_string())
        );
        s.in_flight = Amount::ZERO;
        assert_eq!(in_flight_line(&s), None);
    }

    #[test]
    fn tags_have_css_classes() {
        let t = privacy_tag(&PrivacyTag::Anonymity { estimate: 88 });
        assert_eq!((t.text.as_str(), t.css_class), ("anon set 88", "tag-mixed"));
        let t = privacy_tag(&PrivacyTag::EarlyExit { estimate: 9 });
        assert_eq!(
            (t.text.as_str(), t.css_class),
            ("anon set 9 \u{b7} early exit", "tag-early")
        );
        assert_eq!(privacy_tag(&PrivacyTag::Unmixed).css_class, "tag-unmixed");
        assert_eq!(
            privacy_tag(&PrivacyTag::SmallChange).css_class,
            "tag-change"
        );
    }

    #[test]
    fn queue_row_attainable() {
        let r = queue_row(&qitem(QueueItemStatus::Queued, 35, 60), 50);
        assert_eq!(r.detail, "35 \u{2192} ~60");
        assert_eq!(r.label, "0.1");
        assert!((r.fraction - 0.7).abs() < 1e-9);
    }

    #[test]
    fn queue_row_eligible() {
        let r = queue_row(&qitem(QueueItemStatus::Eligible, 71, 71), 50);
        assert_eq!(r.detail, "71 \u{b7} eligible");
        assert_eq!(r.fraction, 1.0);
        assert_eq!(
            queue_row(&qitem(QueueItemStatus::Queued, 5, 5), 0).fraction,
            1.0
        );
    }

    #[test]
    fn queue_summary_text() {
        let q = QueueState {
            total: btc(0.42),
            anonymity_estimate: 60,
            typical_exit_secs: 86_400,
            waiting_for_saving: false,
            items: vec![],
        };
        assert_eq!(
            queue_summary(&q),
            "Est. anonymity ~60 \u{b7} typical exit within ~1 day"
        );
    }

    #[test]
    fn cover_hidden_unless_checked() {
        let items = vec![
            hist(HistoryKind::Deposit, true, HistoryStatus::Complete),
            hist(HistoryKind::Cover, false, HistoryStatus::Complete),
        ];
        assert_eq!(visible_history(&items, false).len(), 1);
        assert_eq!(visible_history(&items, true).len(), 2);
    }

    #[test]
    fn history_sent_has_nested_change() {
        let k = HistoryKind::Sent {
            change: Some(SentChange {
                amount: btc(0.0313),
                to_spending: true,
            }),
        };
        let r = history_row(&hist(k, false, HistoryStatus::Complete), 1_793_356_920);
        assert!(r.nested.unwrap().contains("change \u{2192} Spending"));
        assert_eq!(r.amount, "\u{2212}0.1000");
    }

    #[test]
    fn small_change_nested_label() {
        let k = HistoryKind::Sent {
            change: Some(SentChange {
                amount: btc(0.0001),
                to_spending: false,
            }),
        };
        let r = history_row(&hist(k, false, HistoryStatus::Complete), 0);
        assert!(r.nested.unwrap().contains("small change"));
    }

    #[test]
    fn history_row_confirming_and_tag() {
        let k = HistoryKind::FromSpending {
            denomination: btc(0.1),
        };
        let st = HistoryStatus::Confirming {
            confirmations: 1,
            required: 3,
        };
        let r = history_row(&hist(k, true, st), 0);
        assert_eq!(r.trailing, "1/3 conf");
        assert_eq!(r.tag.unwrap().text, "0.1");
        assert!(r.nested.is_none());
    }

    #[test]
    fn validation_inline_errors() {
        assert!(send_validation("", "0.1", true).is_err());
        assert!(send_validation("bc1qxyz", "", true).is_err());
        assert!(send_validation("bc1qxyz", "abc", true).is_err());
        assert!(send_validation("bc1qxyz", "0", true).is_err());
        assert!(send_validation("bc1qxyz", "", false).is_ok());
        assert!(send_validation("bc1qxyz", "0.1", true).is_ok());
    }
}
