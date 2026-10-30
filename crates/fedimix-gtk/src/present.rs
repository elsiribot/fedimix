//! Presentation logic: turns core state into display-ready view structs.

use crate::format;
use fedimix_core::api::*;
use fedimix_core::{DestinationKind, Event, parse_btc, parse_destination};

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
        HistoryKind::Cover => (
            "\u{21c4}",
            "Cover traffic",
            Some(TagView {
                text: "cover".to_string(),
                css_class: "tag-cover",
            }),
            None,
        ),
    };
    let trailing = match &item.status {
        HistoryStatus::Confirming {
            confirmations,
            required,
        } => format!("{confirmations}/{required} conf"),
        HistoryStatus::Complete => format::date(item.timestamp, now),
        HistoryStatus::Pending => "pending".to_string(),
        HistoryStatus::Failed => "failed".to_string(),
        HistoryStatus::Refunded => "refunded".to_string(),
        HistoryStatus::Expired => "expired".to_string(),
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
        QueueItemStatus::Deferred { reason } => {
            let why = match reason {
                DeferReason::FeeGuard => "waiting for lower fees",
                DeferReason::FeeUnaffordable => "Spending cannot pay the fee",
                DeferReason::Offline => "waiting to reconnect",
                DeferReason::GracePeriod => "waiting after reconnect",
            };
            format!("{cur} {DOT} {why}")
        }
        QueueItemStatus::Withdrawing { .. } => format!("{cur} {DOT} withdrawing"),
        QueueItemStatus::Landed => format!("{cur} {DOT} landed"),
    };
    QueueRowView {
        label: format::btc_short(item.denomination),
        fraction: fraction(cur.into(), anonymity_target.into()),
        detail,
    }
}

/// Whether "Exit now" can run. The fee guard and the grace period are
/// deferrals of automatic exits only; a manual exit overrides them.
pub fn exit_allowed(status: &QueueItemStatus) -> bool {
    !matches!(
        status,
        QueueItemStatus::Withdrawing { .. }
            | QueueItemStatus::Landed
            | QueueItemStatus::Deferred {
                reason: DeferReason::Offline | DeferReason::FeeUnaffordable,
            }
    )
}

/// Spending hint for exits it cannot pay for (spec section 4).
pub fn spending_hint(q: &QueueState) -> Option<String> {
    let waiting = q
        .items
        .iter()
        .filter(|i| {
            i.status
                == (QueueItemStatus::Deferred {
                    reason: DeferReason::FeeUnaffordable,
                })
        })
        .count();
    match waiting {
        0 => None,
        1 => Some("1 exit waits until Spending can pay its fee".to_string()),
        n => Some(format!("{n} exits wait until Spending can pay their fees")),
    }
}

pub fn queue_summary(q: &QueueState) -> String {
    format!(
        "Est. anonymity ~{} {DOT} typical exit within {}",
        q.anonymity_estimate,
        format::duration_rough(q.typical_exit_secs)
    )
}

/// The text shown to the user for any core error.
pub fn user_message(e: &fedimix_core::FedimixError) -> String {
    use fedimix_core::FedimixError as E;
    match e {
        E::InvalidDestination(why) | E::InvalidAmount(why) | E::NotSetUp(why) => why.clone(),
        E::InsufficientFunds { available, needed } => {
            format!("Not enough funds: {available} available, {needed} needed including the fee")
        }
        E::InvalidInvite(why) => format!("Invalid invite code: {why}"),
        E::UnknownItem(_) => "This item is no longer available".to_string(),
        E::DeviceNotConnected(device) => format!("Connect your {device}"),
        E::Offline => "You are offline. Reconnect and try again".to_string(),
        E::PreviewExpired => "This review expired. Check the details and try again".to_string(),
        E::DeviceRejected => "The device rejected the transaction".to_string(),
        E::Cancelled => "Cancelled".to_string(),
        E::Internal(why) => format!("Something went wrong: {why}"),
    }
}

/// Whether an event can change the snapshots. Progress of an operation does
/// not; its end usually does.
pub fn event_needs_refresh(event: &Event) -> bool {
    match event {
        Event::Operation { state, .. } => state.is_final(),
        Event::Notification(_) => false,
        _ => true,
    }
}

/// What the Send review says while an operation runs. `device` is the name
/// of the signing device.
pub fn operation_caption(state: &OperationState, device: &str) -> String {
    match state {
        OperationState::AwaitingDevice => format!("Waiting for {device}\u{2026}"),
        OperationState::Signing => format!("Signing on {device}\u{2026}"),
        OperationState::Broadcasting => "Broadcasting\u{2026}".to_string(),
        OperationState::Done => "Done".to_string(),
        OperationState::Failed { error } => user_message(error),
        OperationState::Cancelled => "Cancelled".to_string(),
    }
}

/// Whether an operation in `state` can still be stopped.
pub fn operation_cancellable(state: &OperationState) -> bool {
    matches!(
        state,
        OperationState::AwaitingDevice | OperationState::Signing
    )
}

/// Body of the "Exit now?" dialog.
pub fn exit_warning(preview: &ExitPreview) -> String {
    format!(
        "Leaving early lowers your privacy: this note will be withdrawn with a \
         smaller anonymity set. Withdrawal fee: {}.",
        format::sats(preview.fee)
    )
}

/// Whether the amount field is hidden: the destination carries an amount.
pub fn destination_has_amount(dest: &str) -> bool {
    parse_destination(dest).is_ok_and(|d| d.amount.is_some())
}

/// The amount a Send form asks for. `amount` is the field's text; `max` there
/// means everything.
pub fn send_amount(wallet: WalletKind, dest: &str, amount: &str) -> Result<SendAmount, String> {
    if dest.trim().is_empty() {
        return Err("Enter a destination".to_string());
    }
    let parsed = parse_destination(dest).map_err(|e| user_message(&e))?;
    if wallet == WalletKind::Saving && parsed.kind == DestinationKind::Lightning {
        return Err("Saving can only send to a bitcoin address".to_string());
    }
    if parsed.amount.is_some() {
        return Ok(SendAmount::FromInvoice);
    }
    if amount.trim().eq_ignore_ascii_case("max") {
        return Ok(SendAmount::Max);
    }
    match parse_btc(amount).map_err(|e| user_message(&e))? {
        Amount::ZERO => Err("Amount must be greater than zero".to_string()),
        a => Ok(SendAmount::Exact(a)),
    }
}

/// Inline form error. Quiet while the form is still being filled in; a
/// destination the wallet can never pay to is reported at once.
pub fn send_form_error(wallet: WalletKind, dest: &str, amount: &str) -> Option<String> {
    let pays_lightning =
        |d: &str| parse_destination(d).is_ok_and(|p| p.kind == DestinationKind::Lightning);
    if amount.trim().is_empty() && !(wallet == WalletKind::Saving && pays_lightning(dest)) {
        return None;
    }
    send_amount(wallet, dest, amount).err()
}

/// "Expires in ~1 hour", or that it no longer can be paid.
pub fn expiry_line(expires_at: Timestamp, now: Timestamp) -> String {
    if expires_at <= now {
        return "Expired".to_string();
    }
    format!("Expires in {}", format::duration_rough(expires_at - now))
}

/// Review line saying where change goes, if any.
pub fn change_text(change: &fedimix_core::ChangeDestination) -> Option<String> {
    use fedimix_core::ChangeDestination as C;
    match change {
        C::None => None,
        C::Spending { amount } => Some(format!(
            "Change \u{2192} Spending ({})",
            format::btc(*amount)
        )),
        C::SmallChange { amount } => Some(format!(
            "Stays in Saving as small change ({})",
            format::btc(*amount)
        )),
    }
}

/// Label and sensitivity of the review Confirm button.
pub fn confirm_button(
    wallet: fedimix_core::WalletKind,
    device: Option<&fedimix_core::DeviceInfo>,
) -> (String, bool) {
    if wallet == fedimix_core::WalletKind::Spending {
        return ("Send".to_string(), true);
    }
    match device {
        Some(d) if d.connected => (format!("Confirm on {}", d.name), true),
        Some(d) => (format!("Connect your {}", d.name), false),
        None => ("Connect your device".to_string(), false),
    }
}

/// Refill dialog note about the overshoot; empty when there is none.
pub fn refill_overshoot_note(overshoot: Amount) -> String {
    if overshoot == Amount::ZERO {
        return String::new();
    }
    format!(
        "No change output. {} above target rejoins the exit queue.",
        format::btc_plain(overshoot)
    )
}

pub fn visible_history(items: &[HistoryItem], show_cover: bool) -> Vec<HistoryItem> {
    items
        .iter()
        .filter(|i| show_cover || i.kind != HistoryKind::Cover)
        .cloned()
        .collect()
}

/// Raw values of the Settings page, as the widgets report them.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsForm<'a> {
    pub target_btc: &'a str,
    pub anonymity_target: f64,
    pub min_hours: f64,
    pub grace_minutes: f64,
    pub fee_guard_percent: f64,
    pub cover_budget_sat: f64,
    pub electrum: bool,
    pub url: &'a str,
    pub proxy: &'a str,
}

fn in_range(name: &str, v: f64, min: f64, max: f64) -> Result<u64, String> {
    if v.is_finite() && (min..=max).contains(&v) {
        Ok(v.round() as u64)
    } else {
        Err(format!("{name} must be between {min} and {max}"))
    }
}

/// Validate the Settings form and build the new [`Settings`] from it.
pub fn settings_from_form(form: &SettingsForm) -> Result<Settings, String> {
    let url = form.url.trim();
    if url.is_empty() {
        return Err("Enter a server URL".to_string());
    }
    let proxy = form.proxy.trim();
    Ok(Settings {
        spending_target: parse_btc(form.target_btc).map_err(|e| user_message(&e))?,
        anonymity_target: in_range("Anonymity target", form.anonymity_target, 1.0, 1000.0)? as u32,
        min_time_in_pool_secs: in_range("Minimum hours", form.min_hours, 0.0, 720.0)? * 3600,
        online_grace_secs: in_range("Grace minutes", form.grace_minutes, 0.0, 120.0)? * 60,
        fee_guard_percent: in_range("Fee guard", form.fee_guard_percent, 0.0, 100.0)? as u32,
        cover_budget_sat_per_month: in_range("Budget", form.cover_budget_sat, 0.0, 10_000_000.0)?,
        chain_backend: if form.electrum {
            ChainBackend::Electrum {
                url: url.to_string(),
            }
        } else {
            ChainBackend::Esplora {
                url: url.to_string(),
            }
        },
        socks5_proxy: (!proxy.is_empty()).then(|| proxy.to_string()),
    })
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
    fn queue_row_says_why_an_item_is_deferred() {
        let row = |reason| queue_row(&qitem(QueueItemStatus::Deferred { reason }, 5, 9), 50).detail;
        assert_eq!(
            row(DeferReason::FeeGuard),
            "5 \u{b7} waiting for lower fees"
        );
        assert_eq!(
            row(DeferReason::FeeUnaffordable),
            "5 \u{b7} Spending cannot pay the fee"
        );
        assert_eq!(row(DeferReason::Offline), "5 \u{b7} waiting to reconnect");
        assert_eq!(
            row(DeferReason::GracePeriod),
            "5 \u{b7} waiting after reconnect"
        );
    }

    #[test]
    fn exit_allowed_by_status() {
        use DeferReason::*;
        let deferred = |reason| QueueItemStatus::Deferred { reason };
        for allowed in [
            QueueItemStatus::Queued,
            QueueItemStatus::Eligible,
            QueueItemStatus::Scheduled { at: 0 },
            deferred(FeeGuard),
            deferred(GracePeriod),
        ] {
            assert!(exit_allowed(&allowed), "{allowed:?}");
        }
        for refused in [
            QueueItemStatus::Withdrawing { txid: "t".into() },
            QueueItemStatus::Landed,
            deferred(Offline),
            deferred(FeeUnaffordable),
        ] {
            assert!(!exit_allowed(&refused), "{refused:?}");
        }
    }

    #[test]
    fn spending_hint_counts_unaffordable_exits() {
        let queue = |n| QueueState {
            total: Amount::ZERO,
            anonymity_estimate: 0,
            typical_exit_secs: 0,
            waiting_for_saving: false,
            items: std::iter::repeat_n(
                qitem(
                    QueueItemStatus::Deferred {
                        reason: DeferReason::FeeUnaffordable,
                    },
                    1,
                    1,
                ),
                n,
            )
            .chain([qitem(QueueItemStatus::Queued, 1, 1)])
            .collect(),
        };
        assert_eq!(spending_hint(&queue(0)), None);
        assert_eq!(
            spending_hint(&queue(1)).unwrap(),
            "1 exit waits until Spending can pay its fee"
        );
        assert_eq!(
            spending_hint(&queue(2)).unwrap(),
            "2 exits wait until Spending can pay their fees"
        );
    }

    #[test]
    fn history_trailing_for_every_status() {
        let trailing = |status| history_row(&hist(HistoryKind::Paid, false, status), 0).trailing;
        assert_eq!(trailing(HistoryStatus::Pending), "pending");
        assert_eq!(trailing(HistoryStatus::Failed), "failed");
        assert_eq!(trailing(HistoryStatus::Refunded), "refunded");
        assert_eq!(trailing(HistoryStatus::Expired), "expired");
    }

    #[test]
    fn expiry_line_text() {
        assert_eq!(expiry_line(3700, 100), "Expires in ~1 hour");
        assert_eq!(expiry_line(100, 100), "Expired");
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
    fn cover_row_has_cover_tag() {
        let r = history_row(
            &hist(HistoryKind::Cover, false, HistoryStatus::Complete),
            1_793_356_920,
        );
        let tag = r.tag.unwrap();
        assert_eq!((tag.text.as_str(), tag.css_class), ("cover", "tag-cover"));
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
    fn user_messages_are_sentences_without_debug_text() {
        use fedimix_core::FedimixError as E;
        let cases = [
            (E::InvalidDestination("Nope".into()), "Nope"),
            (E::InvalidAmount("Too low".into()), "Too low"),
            (
                E::InsufficientFunds {
                    available: btc(0.46),
                    needed: sat(47_000_420),
                },
                "Not enough funds: 0.46 BTC available, 0.4700042 BTC needed including the fee",
            ),
            (
                E::InsufficientFunds {
                    available: sat(200),
                    needed: sat(420),
                },
                "Not enough funds: 200 sat available, 420 sat needed including the fee",
            ),
            (E::InvalidInvite("bad".into()), "Invalid invite code: bad"),
            (
                E::UnknownItem("x".into()),
                "This item is no longer available",
            ),
            (
                E::DeviceNotConnected("Coldcard".into()),
                "Connect your Coldcard",
            ),
            (E::Offline, "You are offline. Reconnect and try again"),
            (
                E::NotSetUp("Set up Saving first".into()),
                "Set up Saving first",
            ),
            (
                E::PreviewExpired,
                "This review expired. Check the details and try again",
            ),
            (E::DeviceRejected, "The device rejected the transaction"),
            (E::Cancelled, "Cancelled"),
            (E::Internal("boom".into()), "Something went wrong: boom"),
        ];
        for (error, expected) in cases {
            assert_eq!(user_message(&error), expected);
        }
    }

    #[test]
    fn send_form_helpers() {
        use fedimix_core::{ChangeDestination as C, WalletKind as W};
        assert!(destination_has_amount("lnbc1200u1x"));
        assert!(destination_has_amount("BITCOIN:BC1QX?AMOUNT=0.1"));
        assert!(!destination_has_amount("lnbc1x"));
        assert!(!destination_has_amount("bc1qx"));
        assert_eq!(send_form_error(W::Saving, "", ""), None);
        assert_eq!(send_form_error(W::Saving, "bc1q", ""), None);
        assert!(send_form_error(W::Saving, "bc1qx", "abc").is_some());
        assert!(send_form_error(W::Saving, "", "0.1").is_some());
        assert!(send_form_error(W::Spending, "nonsense", "0.1").is_some());
        // No point waiting for an amount: Saving can never pay this.
        assert!(send_form_error(W::Saving, "lnbc1x", "").is_some());
        assert_eq!(send_form_error(W::Spending, "lnbc1x", ""), None);
        assert_eq!(change_text(&C::None), None);
        assert_eq!(
            change_text(&C::Spending { amount: btc(0.1) }).unwrap(),
            "Change \u{2192} Spending (0.1000 BTC)"
        );
        assert!(
            change_text(&C::SmallChange { amount: sat(5000) })
                .unwrap()
                .starts_with("Stays in Saving as small change")
        );
    }

    #[test]
    fn confirm_button_states() {
        use fedimix_core::{DeviceInfo, WalletKind as W};
        let dev = |connected| DeviceInfo {
            id: "c".into(),
            name: "Coldcard".into(),
            kind: fedimix_core::DeviceKind::Coldcard,
            connected,
        };
        assert_eq!(confirm_button(W::Spending, None), ("Send".into(), true));
        assert_eq!(
            confirm_button(W::Saving, Some(&dev(true))),
            ("Confirm on Coldcard".into(), true)
        );
        assert_eq!(
            confirm_button(W::Saving, Some(&dev(false))),
            ("Connect your Coldcard".into(), false)
        );
        assert!(!confirm_button(W::Saving, None).1);
    }

    #[test]
    fn send_amount_from_the_form() {
        use fedimix_core::WalletKind::{Saving, Spending};
        assert!(send_amount(Spending, "", "0.1").is_err());
        assert!(send_amount(Spending, "bc1qxyz", "").is_err());
        assert!(send_amount(Spending, "bc1qxyz", "abc").is_err());
        assert!(send_amount(Spending, "bc1qxyz", "0").is_err());
        assert_eq!(
            send_amount(Spending, "bc1qxyz", "0.1"),
            Ok(SendAmount::Exact(btc(0.1)))
        );
        assert_eq!(send_amount(Saving, "bc1qxyz", " MAX "), Ok(SendAmount::Max));
        assert_eq!(
            send_amount(Spending, "lnbc1200u1x", ""),
            Ok(SendAmount::FromInvoice)
        );
        assert_eq!(
            send_amount(Saving, "bitcoin:bc1qx?amount=0.1", ""),
            Ok(SendAmount::FromInvoice)
        );
        assert_eq!(
            send_amount(Spending, "lnbc1x", "0.1"),
            Ok(SendAmount::Exact(btc(0.1)))
        );
        assert!(send_amount(Saving, "lnbc1x", "0.1").is_err());
    }

    #[test]
    fn only_final_operation_states_refresh() {
        let op = |state| Event::Operation {
            id: "a".into(),
            kind: OperationKind::Send,
            state,
        };
        assert!(!event_needs_refresh(&op(OperationState::Signing)));
        assert!(event_needs_refresh(&op(OperationState::Done)));
        assert!(event_needs_refresh(&Event::QueueChanged));
        assert!(!event_needs_refresh(&Event::Notification(
            fedimix_core::Notice::SendConfirmed {
                amount: Amount::ZERO
            }
        )));
    }

    #[test]
    fn operation_captions() {
        use OperationState as S;
        let text = |s: S| operation_caption(&s, "Coldcard");
        assert_eq!(text(S::AwaitingDevice), "Waiting for Coldcard\u{2026}");
        assert_eq!(text(S::Signing), "Signing on Coldcard\u{2026}");
        assert_eq!(text(S::Broadcasting), "Broadcasting\u{2026}");
        assert_eq!(
            text(S::Failed {
                error: fedimix_core::FedimixError::DeviceRejected
            }),
            "The device rejected the transaction"
        );
        assert!(operation_cancellable(&S::AwaitingDevice));
        assert!(operation_cancellable(&S::Signing));
        assert!(!operation_cancellable(&S::Broadcasting));
    }

    #[test]
    fn refill_overshoot_note_text() {
        assert_eq!(
            refill_overshoot_note(sat(2_120_000)),
            "No change output. 0.0212 above target rejoins the exit queue."
        );
        assert_eq!(refill_overshoot_note(Amount::ZERO), "");
    }

    /// Same call sequence the UI uses, checked through the presentation layer.
    #[tokio::test]
    async fn demo_flow_presentation() {
        use fedimix_core::{
            DemoBackend, DemoScenario, DemoTiming, EventStream, Fedimix, SendRequest,
        };
        use std::time::Duration;
        const NOW: Timestamp = 1_793_318_400;
        const FAST: DemoTiming = DemoTiming {
            step: Duration::from_millis(1),
            preview_ttl: Duration::from_secs(600),
        };
        let demo = |scenario| Fedimix::new(DemoBackend::with_timing(scenario, FAST));
        async fn done(events: &EventStream, id: OperationId) {
            loop {
                if let Some(Event::Operation { id: got, state, .. }) = events.next().await
                    && got == id
                    && state.is_final()
                {
                    assert_eq!(state, OperationState::Done);
                    return;
                }
            }
        }

        // First run: join, then set up Saving.
        let core = demo(DemoScenario::FirstRun);
        let invite = core.suggested_federations().await.unwrap()[0]
            .invite
            .clone();
        core.join_federation(invite).await.unwrap();
        let device = core.detect_devices().await.unwrap().remove(0);
        core.setup_saving(device.id).await.unwrap();
        let sp = core.spending_state().await.unwrap();
        assert_eq!(saving_banner(&sp), None);
        assert!(sp.federation.is_some());

        // Funded: caption, send review, history rows, exit now.
        let core = demo(DemoScenario::Funded);
        let events = core.subscribe();
        let sp = core.spending_state().await.unwrap();
        let caption = spending_caption(&sp);
        assert!(caption.opens_queue && !caption.shows_refill);
        let saving = core.saving_state().await.unwrap();
        assert!(in_flight_line(&saving).is_some());
        let preview = core
            .prepare_send(SendRequest {
                wallet: WalletKind::Saving,
                destination: "bc1qdemo0destination".into(),
                amount: SendAmount::Exact(Amount::from_sat(1_000_000)),
                fee_rate_sat_vb: None,
            })
            .await
            .unwrap();
        assert!(change_text(&preview.change).is_some());
        let (label, enabled) = confirm_button(WalletKind::Saving, saving.device.as_ref());
        assert!(enabled && label.starts_with("Confirm on "));
        done(&events, core.confirm_send(preview.id).await.unwrap()).await;
        let saving = core.saving_state().await.unwrap();
        let row = history_row(&saving.history[0], NOW);
        assert_eq!(row.title, "Sent");
        assert!(row.amount.starts_with('-') || row.amount.starts_with('\u{2212}'));

        let q = core.queue_state().await.unwrap();
        let n = q.items.len();
        let rows: Vec<_> = q.items.iter().map(|i| queue_row(i, 50)).collect();
        assert!(rows.iter().all(|r| (0.0..=1.0).contains(&r.fraction)));
        let exit = core.prepare_exit(q.items[0].id.clone()).await.unwrap();
        assert!(exit_warning(&exit).contains("Withdrawal fee: 420 sat"));
        done(&events, core.exit_now(exit.id).await.unwrap()).await;
        let q = core.queue_state().await.unwrap();
        assert_eq!(q.items.len(), n - 1);
        assert!(queue_summary(&q).contains("typical exit"));

        // Below target: refill moves the caption to "for spending".
        let core = demo(DemoScenario::BelowTarget);
        let events = core.subscribe();
        let sp = core.spending_state().await.unwrap();
        assert!(spending_caption(&sp).shows_refill);
        let p = core.prepare_refill().await.unwrap();
        done(&events, core.confirm_refill(p.id).await.unwrap()).await;
        let sp = core.spending_state().await.unwrap();
        let caption = spending_caption(&sp);
        assert!(!caption.shows_refill);
        assert!(caption.text.contains("for spending"), "{}", caption.text);
        assert_eq!(visible_history(&sp.history, false).len(), sp.history.len());
    }
}

#[cfg(test)]
mod settings_form_tests {
    use super::*;

    fn form() -> SettingsForm<'static> {
        SettingsForm {
            target_btc: "0.05",
            anonymity_target: 100.0,
            min_hours: 12.0,
            grace_minutes: 5.0,
            fee_guard_percent: 3.0,
            cover_budget_sat: 30_000.0,
            electrum: true,
            url: " ssl://electrum.example:50002 ",
            proxy: "",
        }
    }

    #[test]
    fn accepts_valid_form() {
        let s = settings_from_form(&form()).unwrap();
        assert_eq!(s.spending_target, Amount::from_sat(5_000_000));
        assert_eq!(s.anonymity_target, 100);
        assert_eq!(s.min_time_in_pool_secs, 12 * 3600);
        assert_eq!(s.online_grace_secs, 300);
        assert_eq!(s.fee_guard_percent, 3);
        assert_eq!(s.cover_budget_sat_per_month, 30_000);
        assert_eq!(
            s.chain_backend,
            ChainBackend::Electrum {
                url: "ssl://electrum.example:50002".into()
            }
        );
        assert_eq!(s.socks5_proxy, None);
    }

    #[test]
    fn proxy_is_trimmed_and_kept() {
        let mut f = form();
        f.proxy = " 127.0.0.1:9050 ";
        let s = settings_from_form(&f).unwrap();
        assert_eq!(s.socks5_proxy.as_deref(), Some("127.0.0.1:9050"));
    }

    #[test]
    fn rejects_bad_target() {
        for bad in ["", "abc", "-1", "21000001"] {
            let mut f = form();
            f.target_btc = bad;
            let err = settings_from_form(&f).unwrap_err();
            assert!(!err.is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn rejects_out_of_range_numbers_and_empty_url() {
        let mut f = form();
        f.anonymity_target = 0.0;
        assert!(settings_from_form(&f).is_err());
        let mut f = form();
        f.fee_guard_percent = 101.0;
        assert!(settings_from_form(&f).is_err());
        let mut f = form();
        f.url = "  ";
        assert!(settings_from_form(&f).is_err());
    }
}
