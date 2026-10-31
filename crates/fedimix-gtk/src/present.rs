//! Presentation logic: turns core state into display-ready view structs.

use crate::format;
use fedimix_core::api::*;
use fedimix_core::{DestinationKind, Event, parse_btc, parse_destination};

const DOT: &str = "\u{b7}";
const CHEVRON: &str = "\u{203a}";

#[derive(Debug, Clone, PartialEq)]
pub struct Caption {
    /// Plain caption; empty when the link says it all.
    pub text: String,
    /// Link to the Moving to Saving page.
    pub queue_link: Option<String>,
    pub shows_refill: bool,
    /// Bar fill, always within 0..=1.
    pub fraction: f64,
    pub shows_bar: bool,
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
    /// Fee and transaction, to read or copy.
    pub detail: Option<String>,
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
    // With nowhere for money to go, a target would imply that it is going.
    if !s.features.saving {
        return Caption {
            text: String::new(),
            queue_link: None,
            shows_refill: false,
            fraction: 0.0,
            shows_bar: false,
        };
    }
    let spend = format::btc_short(s.for_spending);
    let moving = format::btc_short(s.moving);
    let is_moving = s.moving > Amount::ZERO;
    let moving_link = format!("{moving} moving to Saving {CHEVRON}");
    let waiting = format!("{moving} waiting for Saving");
    if s.for_spending < s.target {
        let target = format!("{spend} of {} target", format::btc_short(s.target));
        return Caption {
            text: if is_moving && !s.saving_ready {
                format!("{target} {DOT} {waiting}")
            } else {
                target
            },
            queue_link: (is_moving && s.saving_ready).then_some(moving_link),
            shows_refill: s.saving_ready,
            fraction: fraction(s.for_spending.msat, s.target.msat),
            shows_bar: true,
        };
    }
    if !is_moving {
        return Caption {
            text: format!("{spend} for spending"),
            queue_link: None,
            shows_refill: false,
            fraction: 1.0,
            shows_bar: true,
        };
    }
    let total = s.for_spending.msat.saturating_add(s.moving.msat);
    let fraction = fraction(s.for_spending.msat, total);
    if s.saving_ready {
        Caption {
            text: String::new(),
            queue_link: Some(format!("{spend} for spending {DOT} {moving_link}")),
            shows_refill: false,
            fraction,
            shows_bar: true,
        }
    } else {
        Caption {
            text: format!("{spend} for spending {DOT} {waiting}"),
            queue_link: None,
            shows_refill: false,
            fraction,
            shows_bar: true,
        }
    }
}

pub fn saving_banner(s: &SpendingState, can_set_up: bool) -> Option<String> {
    (can_set_up && !s.saving_ready && s.moving > Amount::ZERO).then(|| {
        format!(
            "Set up Saving to move {} BTC to cold storage",
            format::btc_short(s.moving)
        )
    })
}

/// What the Saving tab shows while Saving is not set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavingEmptyState {
    pub title: &'static str,
    pub button: &'static str,
    pub button_enabled: bool,
    pub caption: Option<&'static str>,
}

pub fn saving_empty_state(can_set_up: bool) -> SavingEmptyState {
    if can_set_up {
        SavingEmptyState {
            title: "Connect a hardware wallet",
            button: "Set up",
            button_enabled: true,
            caption: None,
        }
    } else {
        SavingEmptyState {
            title: "Saving",
            button: "Connect a hardware wallet",
            button_enabled: false,
            caption: Some("Available in a later version"),
        }
    }
}

pub const RECOVERING_BANNER: &str =
    "Restoring your wallet from the federation. This can take a while.";

/// The banner about the connection to `federation`, if it needs one. A problem
/// comes first: waiting will not cure it, so the user has to hear it.
pub fn connection_banner(federation: Option<&FederationInfo>) -> Option<&str> {
    match federation {
        Some(FederationInfo {
            problem: Some(problem),
            ..
        }) => Some(problem),
        Some(f) if f.recovering => Some(RECOVERING_BANNER),
        Some(f) if !f.online => Some("Offline"),
        _ => None,
    }
}

pub const BACKUP_BANNER: &str = "Write down your recovery words and keep your invite codes";

/// Above the words. In this version they restore nothing on their own, and a
/// folder copy that runs again reuses ecash nonces.
pub const RECOVERY_WARNING: &str = "Write these words down and keep them offline. Anyone who \
has them can take your bitcoin. Never share or photograph them. In this version Fedimix \
cannot restore a wallet from them alone: keep your invite codes too, because the words do not \
say which federations you joined. Never run or restore an old copy of the data folder: that \
can lose your ecash.";

/// A user can hold real funds with no backup, so ask once there is something
/// to lose, until the words have been opened.
pub fn backup_banner(balance: Amount, words_shown: bool) -> bool {
    balance > Amount::ZERO && !words_shown
}

/// Under a federation's name in Settings: what it holds, how it is doing, and
/// its id, as a name is all a federation chooses for itself.
pub fn federation_subtitle(f: &FederationInfo) -> String {
    let status = match (&f.problem, f.online) {
        (Some(problem), _) => problem.as_str(),
        (None, true) => "Online",
        (None, false) => "Offline",
    };
    format!("{} {DOT} {status}\n{}", format::btc(f.balance), f.id)
}

/// An invite as Settings offers it: a short form to read, the whole code to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteOffer {
    pub shown: String,
    pub copy: String,
}

/// Longest invite shown whole; a longer one keeps its ends.
const INVITE_SHOWN_WHOLE: usize = 24;

/// `None` when the federation's invite is not known.
pub fn federation_invite(f: &FederationInfo) -> Option<InviteOffer> {
    if f.invite.is_empty() {
        return None;
    }
    let chars: Vec<char> = f.invite.chars().collect();
    let shown = if chars.len() <= INVITE_SHOWN_WHOLE {
        f.invite.clone()
    } else {
        let head: String = chars[..12].iter().collect();
        let tail: String = chars[chars.len() - 8..].iter().collect();
        format!("{head}\u{2026}{tail}")
    };
    Some(InviteOffer {
        shown,
        copy: f.invite.clone(),
    })
}

/// What to say once a join has worked, if anything.
pub fn join_notice(f: &FederationInfo) -> Option<String> {
    (!f.has_lightning)
        .then(|| "This federation has no Lightning: only on-chain works here.".to_string())
}

/// The Join page's text. Without suggestions the invite is the only way in.
pub fn join_description(has_suggestions: bool) -> &'static str {
    if has_suggestions {
        "Fedimix keeps your spending money as ecash in a federation."
    } else {
        "Fedimix keeps your spending money as ecash in a federation. Paste an invite code from \
         one to join it."
    }
}

/// Saving tab status line. An unplugged device stays named, so the user
/// knows what to reach for.
pub fn device_line(d: &DeviceInfo) -> String {
    if d.connected {
        d.name.clone()
    } else {
        format!("{} {DOT} Not connected", d.name)
    }
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
        detail: history_detail(item),
    }
}

/// Only a finished row has a fee that was paid.
fn history_detail(item: &HistoryItem) -> Option<String> {
    if item.status != HistoryStatus::Complete {
        return None;
    }
    let fee = item.fee.map(|fee| format!("Fee {}", format::sats(fee)));
    let txid = item.txid.as_ref().map(|txid| format!("Txid {txid}"));
    let parts: Vec<String> = fee.into_iter().chain(txid).collect();
    (!parts.is_empty()).then(|| parts.join(&format!(" {DOT} ")))
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

/// Whether "Exit now" can run. The fee guard is a deferral of automatic exits
/// only, so a manual exit overrides it. The online grace period is not: no
/// exit happens within it (spec section 1), and the core refuses one too.
pub fn exit_allowed(status: &QueueItemStatus) -> bool {
    !matches!(
        status,
        QueueItemStatus::Withdrawing { .. }
            | QueueItemStatus::Landed
            | QueueItemStatus::Deferred {
                reason: DeferReason::Offline
                    | DeferReason::FeeUnaffordable
                    | DeferReason::GracePeriod,
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

/// A complete sentence, as the core writes them, rather than a bare reason.
fn is_sentence(text: &str) -> bool {
    text.ends_with('.') && text.chars().next().is_some_and(char::is_uppercase)
}

/// `why` if it can stand alone, otherwise `lead` in front of it.
fn sentence_or(lead: &str, why: &str) -> String {
    if is_sentence(why) {
        why.to_string()
    } else {
        format!("{lead}: {why}")
    }
}

/// The text shown to the user for any core error. The core's sentences say
/// what happened and what to do, so they go through unchanged.
pub fn user_message(e: &fedimix_core::FedimixError) -> String {
    use fedimix_core::FedimixError as E;
    match e {
        E::InvalidDestination(why) | E::InvalidAmount(why) | E::NotSetUp(why) => why.clone(),
        E::InsufficientFunds { available, needed } => {
            format!("Not enough funds: {available} available, {needed} needed including the fee")
        }
        E::InvalidInvite(why) => sentence_or("Invalid invite code", why),
        E::UnknownItem(_) => "This item is no longer available".to_string(),
        E::DeviceNotConnected(device) => format!("Connect your {device}"),
        E::Offline => "You are offline. Reconnect and try again".to_string(),
        E::ProxyUnsupported | E::WrongNetwork { .. } => e.to_string(),
        E::PreviewExpired => "This review expired. Check the details and try again".to_string(),
        E::DeviceRejected => "The device rejected the transaction".to_string(),
        E::Cancelled => "Cancelled".to_string(),
        E::GracePeriod => "Exits wait until the app has been online for a while.".to_string(),
        E::Internal(why) => sentence_or("Something went wrong", why),
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

/// How long a Send may sit in Broadcasting before its dialog can be closed.
pub const SEND_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Replaces the Broadcasting caption once the dialog can be closed.
pub const SEND_UNLOCKED_CAPTION: &str =
    "You can close this; the result will appear when it arrives";

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

/// Whether the review has inputs or change to disclose.
pub fn review_discloses(preview: &SendPreview) -> bool {
    !preview.inputs.is_empty() || change_text(&preview.change).is_some()
}

/// A Spending fee is the gateway's and the federation's, not a miner's.
pub fn fee_label(wallet: WalletKind) -> &'static str {
    match wallet {
        WalletKind::Spending => "Fee",
        WalletKind::Saving => "Network fee",
    }
}

/// What leaves the wallet: the amount and the fee.
pub fn review_total(preview: &SendPreview) -> String {
    let total = preview.amount.checked_add(preview.fee);
    format::btc_exact(total.unwrap_or(Amount { msat: u64::MAX }))
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

/// How many rows each "Show more" adds.
pub const HISTORY_PAGE: u32 = fedimix_core::SNAPSHOT_HISTORY as u32;

/// A snapshot with fewer rows than it can hold has all of them; a full one
/// may have older rows behind it.
pub fn history_may_have_more(snapshot_len: usize) -> bool {
    snapshot_len >= fedimix_core::SNAPSHOT_HISTORY
}

/// Whether pages loaded below `prev` still continue from the end of `next`.
/// A new row pushes the last one out of the snapshot, which would leave a
/// gap between it and the pages: they must be dropped then.
pub fn extra_pages_line_up(prev: &[HistoryItem], next: &[HistoryItem]) -> bool {
    prev.len() == next.len() && prev.iter().zip(next).all(|(a, b)| a.id == b.id)
}

/// The rows to draw: the snapshot, then the pages loaded below it.
pub fn history_to_show(
    snapshot: &[HistoryItem],
    pages: &[HistoryItem],
    show_cover: bool,
) -> Vec<HistoryItem> {
    let all: Vec<HistoryItem> = snapshot.iter().chain(pages).cloned().collect();
    visible_history(&all, show_cover)
}

/// Under the invoice or the address: what the user should know before anyone
/// pays it. Only a repeating address has a minimum that moves with the feerate.
pub fn receive_note(info: &ReceiveInfo) -> Option<String> {
    if let Some(fee) = info.fee {
        return Some(format!(
            "Fees take about {} of this amount.",
            format::sats(fee)
        ));
    }
    let min = info.min_deposit.filter(|m| *m != Amount::ZERO);
    let repeats = "This address stays the same until it receives a payment.";
    match (min, info.address_repeats) {
        (Some(min), true) => Some(format!(
            "Send well above {}; this minimum follows Bitcoin fees. {repeats}",
            format::sats(min)
        )),
        (None, true) => Some(repeats.to_string()),
        (Some(min), false) => Some(format!(
            "Deposits of {} or less are not credited.",
            format::sats(min)
        )),
        (None, false) => None,
    }
}

/// What to tell about an operation found running at startup, once it ends.
/// Its dialog is gone, so a toast is the only report.
pub fn resumed_outcome(kind: OperationKind, state: &OperationState) -> Option<String> {
    match state {
        OperationState::Done => Some(
            match kind {
                OperationKind::Send => "Payment sent",
                OperationKind::Refill => "Refill done",
                OperationKind::Exit => "Exit done",
            }
            .to_string(),
        ),
        OperationState::Failed { error } => Some(user_message(error)),
        OperationState::Cancelled => Some("Cancelled".to_string()),
        _ => None,
    }
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
    /// The page shows the server rows. When it does not, `electrum` and `url`
    /// are the values in force and are kept as they are, empty or not.
    pub servers_shown: bool,
}

fn in_range(name: &str, v: f64, min: f64, max: f64) -> Result<u64, String> {
    if v.is_finite() && (min..=max).contains(&v) {
        Ok(v.round() as u64)
    } else {
        Err(format!("{name} must be between {min} and {max}"))
    }
}

/// `host[:port]`, or `[ipv6][:port]`.
fn valid_host_port(authority: &str, port_required: bool) -> bool {
    let host_ok = |h: &str| {
        !h.is_empty()
            && h.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    };
    let port_ok = |p: &str| p.parse::<u16>().is_ok_and(|p| p != 0);
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return false;
        };
        let good_host =
            !host.is_empty() && host.bytes().all(|b| b.is_ascii_hexdigit() || b == b':');
        return good_host
            && match tail.strip_prefix(':') {
                Some(port) => port_ok(port),
                None => tail.is_empty() && !port_required,
            };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => host_ok(host) && port_ok(port),
        None => host_ok(authority) && !port_required,
    }
}

/// Esplora: `http(s)://host[:port][/path]`. Electrum: `ssl|tcp://host:port`.
pub fn validate_server_url(electrum: bool, url: &str) -> Result<(), String> {
    let url = url.trim();
    if electrum {
        let authority = url
            .strip_prefix("ssl://")
            .or_else(|| url.strip_prefix("tcp://"));
        return match authority {
            Some(a) if valid_host_port(a, true) => Ok(()),
            _ => Err("Enter the server as ssl://host:port or tcp://host:port".to_string()),
        };
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("Server URL must start with https:// or http://")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if valid_host_port(authority, false) && !path.contains(char::is_whitespace) {
        Ok(())
    } else {
        Err("Server URL needs a valid host, for example https://host/api".to_string())
    }
}

/// Under the proxy row: fedimint has no proxy support, so nothing connects.
pub const PROXY_CAPTION: &str = "With a proxy set, Fedimix stays offline.";

/// Empty, or `[socks5://|socks5h://]host:port`. A proxy that does not parse
/// must never be dropped in favour of a direct connection.
pub fn validate_proxy(proxy: &str) -> Result<(), String> {
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return Ok(());
    }
    let authority = proxy
        .strip_prefix("socks5://")
        .or_else(|| proxy.strip_prefix("socks5h://"))
        .unwrap_or(proxy);
    if valid_host_port(authority, true) {
        Ok(())
    } else {
        Err("Enter the proxy as host:port, for example 127.0.0.1:9050".to_string())
    }
}

/// Validate the Settings form and build the new [`Settings`] from it.
pub fn settings_from_form(form: &SettingsForm) -> Result<Settings, String> {
    let url = form.url.trim();
    if form.servers_shown {
        if url.is_empty() {
            return Err("Enter a server URL".to_string());
        }
        validate_server_url(form.electrum, url)?;
    }
    let proxy = form.proxy.trim();
    validate_proxy(proxy)?;
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
        // Not on the form; the page keeps what it loaded.
        recovery_words_shown: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview() -> SendPreview {
        SendPreview {
            id: "p".into(),
            wallet: WalletKind::Spending,
            destination: "d".into(),
            amount: sat(1_000),
            fee: sat(10),
            federation: Some("f".into()),
            inputs: Vec::new(),
            change: ChangeDestination::None,
            expires_at: 0,
        }
    }

    #[test]
    fn a_review_with_nothing_to_disclose_says_so() {
        assert!(!review_discloses(&preview()));
        let change = SendPreview {
            change: ChangeDestination::Spending { amount: sat(1) },
            ..preview()
        };
        assert!(review_discloses(&change));
        let input = SendPreview {
            inputs: vec![SendInput {
                outpoint_or_note: "notes".into(),
                amount: sat(1_010),
                tag: None,
            }],
            ..preview()
        };
        assert!(review_discloses(&input));
    }

    #[test]
    fn a_review_names_its_fee_and_the_real_debit() {
        assert_eq!(fee_label(WalletKind::Spending), "Fee");
        assert_eq!(fee_label(WalletKind::Saving), "Network fee");
        assert_eq!(review_total(&preview()), format::btc_exact(sat(1_010)));
    }

    #[test]
    fn proxy_caption_states_the_limit() {
        assert_eq!(PROXY_CAPTION, "With a proxy set, Fedimix stays offline.");
    }

    #[test]
    fn without_saving_spending_shows_the_balance_alone() {
        let state = SpendingState {
            features: Features::NONE,
            ..spending(0.05, 0.42, 0.05, false)
        };
        let caption = spending_caption(&state);
        assert_eq!(caption.text, "");
        assert_eq!(caption.queue_link, None);
        assert!(!caption.shows_refill);
        assert!(!caption.shows_bar);
        assert!(spending_caption(&spending(0.05, 0.42, 0.05, false)).shows_bar);
    }

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
            total_balance: Amount::ZERO,
            features: Features::ALL,
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
            txid: None,
            fee: None,
            session: None,
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
        assert_eq!(c.text, "");
        assert_eq!(
            c.queue_link.as_deref(),
            Some("0.05 for spending \u{b7} 0.42 moving to Saving \u{203a}")
        );
        assert!(!c.shows_refill);
        assert!((c.fraction - 0.106).abs() < 0.001);
    }

    #[test]
    fn caption_below_target() {
        let c = spending_caption(&spending(0.0112, 0.0, 0.05, true));
        assert_eq!(c.text, "0.0112 of 0.05 target");
        assert_eq!(c.queue_link, None);
        assert!(c.shows_refill);
        assert!((c.fraction - 0.224).abs() < 0.001);
    }

    #[test]
    fn caption_below_target_keeps_the_queue() {
        let c = spending_caption(&spending(0.05, 0.3, 5.0, true));
        assert_eq!(c.text, "0.05 of 5 target");
        assert_eq!(
            c.queue_link.as_deref(),
            Some("0.3 moving to Saving \u{203a}")
        );
        assert!(c.shows_refill);
    }

    #[test]
    fn no_refill_without_saving() {
        let c = spending_caption(&spending(0.01, 0.0, 0.05, false));
        assert_eq!(c.text, "0.01 of 0.05 target");
        assert!(!c.shows_refill);
        let c = spending_caption(&spending(0.01, 0.2, 0.05, false));
        assert_eq!(c.text, "0.01 of 0.05 target \u{b7} 0.2 waiting for Saving");
        assert_eq!(c.queue_link, None);
        assert!(!c.shows_refill);
    }

    #[test]
    fn caption_waiting_for_saving() {
        let c = spending_caption(&spending(0.05, 0.42, 0.05, false));
        assert_eq!(c.text, "0.05 for spending \u{b7} 0.42 waiting for Saving");
        assert_eq!(c.queue_link, None);
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
            saving_banner(&spending(0.05, 0.42, 0.05, false), true),
            Some("Set up Saving to move 0.42 BTC to cold storage".to_string())
        );
        assert_eq!(saving_banner(&spending(0.05, 0.42, 0.05, true), true), None);
        assert_eq!(saving_banner(&spending(0.05, 0.0, 0.05, false), true), None);
    }

    #[test]
    fn device_line_says_when_unplugged() {
        let mut d = DeviceInfo {
            id: "c".into(),
            name: "Coldcard".into(),
            kind: fedimix_core::DeviceKind::Coldcard,
            connected: true,
        };
        assert_eq!(device_line(&d), "Coldcard");
        d.connected = false;
        assert_eq!(device_line(&d), "Coldcard \u{b7} Not connected");
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
    fn exit_refused_during_grace_period() {
        let grace = QueueItemStatus::Deferred {
            reason: DeferReason::GracePeriod,
        };
        assert!(!exit_allowed(&grace));
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
        ] {
            assert!(exit_allowed(&allowed), "{allowed:?}");
        }
        for refused in [
            QueueItemStatus::Withdrawing { txid: None },
            QueueItemStatus::Landed,
            deferred(Offline),
            deferred(FeeUnaffordable),
            deferred(GracePeriod),
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
    fn core_sentences_reach_the_user_unchanged() {
        use fedimix_core::FedimixError as E;
        let lost = "Fedimix lost its record of this payment and cannot tell whether it was sent. \
                    Check your balance before paying again.";
        for sentence in [
            lost,
            "The Lightning gateway did not take the payment.",
            "Fedimix is already running with this data folder.",
        ] {
            assert_eq!(user_message(&E::Internal(sentence.into())), sentence);
        }
        assert_eq!(
            user_message(&E::InvalidInvite(
                "This federation runs on a different Bitcoin network.".into()
            )),
            "This federation runs on a different Bitcoin network."
        );
        assert_eq!(
            user_message(&E::ProxyUnsupported),
            "Fedimix cannot reach federations through a proxy yet. Clear the proxy in Settings to connect."
        );
        assert_eq!(
            user_message(&E::WrongNetwork {
                folder: "regtest".into(),
                wanted: "bitcoin".into()
            }),
            "This data folder is for regtest, but Fedimix was started for bitcoin. Start it for regtest, or choose another data folder."
        );
        assert_eq!(
            user_message(&E::GracePeriod),
            "Exits wait until the app has been online for a while."
        );
    }

    #[test]
    fn saving_empty_state_without_setup() {
        let real = saving_empty_state(false);
        assert_eq!(real.button, "Connect a hardware wallet");
        assert!(!real.button_enabled);
        assert_eq!(real.caption, Some("Available in a later version"));
        assert_ne!(real.title, real.button, "the page must not say it twice");

        let demo = saving_empty_state(true);
        assert_eq!(demo.title, "Connect a hardware wallet");
        assert_eq!(demo.button, "Set up");
        assert!(demo.button_enabled);
        assert_eq!(demo.caption, None);
    }

    #[test]
    fn no_setup_banner_while_setup_is_unavailable() {
        let sp = spending(0.05, 0.42, 0.05, false);
        assert!(saving_banner(&sp, true).is_some());
        assert_eq!(saving_banner(&sp, false), None);
    }

    #[test]
    fn a_finished_row_shows_its_fee_and_txid() {
        let done = HistoryItem {
            txid: Some("abc".into()),
            fee: Some(sat(300)),
            ..hist(
                HistoryKind::Sent { change: None },
                false,
                HistoryStatus::Complete,
            )
        };
        assert_eq!(
            history_row(&done, 0).detail.as_deref(),
            Some("Fee 300 sat \u{b7} Txid abc")
        );
        let lightning = HistoryItem {
            txid: None,
            ..done.clone()
        };
        assert_eq!(
            history_row(&lightning, 0).detail.as_deref(),
            Some("Fee 300 sat")
        );
        let failed = HistoryItem {
            status: HistoryStatus::Failed,
            ..done
        };
        assert_eq!(
            history_row(&failed, 0).detail,
            None,
            "that fee was not paid"
        );
    }

    fn federation_info() -> FederationInfo {
        FederationInfo {
            id: "abc123".into(),
            name: "Bob & Alice".into(),
            online: true,
            anonymity_estimate: 0,
            balance: sat(1_230_000),
            recovering: false,
            problem: None,
            has_lightning: true,
            invite: "fed1abc".into(),
        }
    }

    #[test]
    fn federation_row_offers_its_invite() {
        let long = "fed1".to_string() + &"q".repeat(120);
        let fed = FederationInfo {
            invite: long.clone(),
            ..federation_info()
        };
        let offer = federation_invite(&fed).unwrap();
        assert_eq!(offer.copy, long);
        assert!(offer.shown.starts_with("fed1qqq"), "{}", offer.shown);
        assert!(offer.shown.ends_with("qqq"), "{}", offer.shown);
        assert!(offer.shown.chars().count() < 40, "{}", offer.shown);
        let short = FederationInfo {
            invite: "fed1abc".into(),
            ..federation_info()
        };
        assert_eq!(federation_invite(&short).unwrap().shown, "fed1abc");
        let none = FederationInfo {
            invite: String::new(),
            ..federation_info()
        };
        assert_eq!(federation_invite(&none), None);
    }

    #[test]
    fn a_federation_row_says_what_it_holds_and_which_one_it_is() {
        assert_eq!(
            federation_subtitle(&federation_info()),
            "0.0123 BTC \u{b7} Online\nabc123"
        );
        let offline = FederationInfo {
            online: false,
            ..federation_info()
        };
        assert_eq!(
            federation_subtitle(&offline),
            "0.0123 BTC \u{b7} Offline\nabc123"
        );
        let broken = FederationInfo {
            problem: Some("It broke.".into()),
            ..offline
        };
        assert_eq!(
            federation_subtitle(&broken),
            "0.0123 BTC \u{b7} It broke.\nabc123"
        );
    }

    #[test]
    fn a_join_without_lightning_says_so() {
        assert_eq!(join_notice(&federation_info()), None);
        let dark = FederationInfo {
            has_lightning: false,
            ..federation_info()
        };
        assert_eq!(
            join_notice(&dark).as_deref(),
            Some("This federation has no Lightning: only on-chain works here.")
        );
    }

    #[test]
    fn the_recovery_warning_promises_only_what_is_true() {
        assert!(RECOVERY_WARNING.contains("cannot restore a wallet from them alone"));
        assert!(RECOVERY_WARNING.contains("invite codes"));
        assert!(RECOVERY_WARNING.contains("old copy"));
        assert!(!RECOVERY_WARNING.contains("cannot be recovered"));
        assert!(BACKUP_BANNER.contains("invite codes"));
    }

    #[test]
    fn restoring_replaces_the_offline_banner() {
        let fed = |online, recovering| FederationInfo {
            id: "f".into(),
            name: "F".into(),
            online,
            anonymity_estimate: 0,
            balance: Amount::ZERO,
            recovering,
            problem: None,
            has_lightning: true,
            invite: String::new(),
        };
        assert_eq!(connection_banner(None), None);
        assert_eq!(connection_banner(Some(&fed(true, false))), None);
        assert_eq!(connection_banner(Some(&fed(false, false))), Some("Offline"));
        let problem = FederationInfo {
            problem: Some("This federation's data is damaged.".into()),
            ..fed(false, false)
        };
        assert_eq!(
            connection_banner(Some(&problem)),
            Some("This federation's data is damaged.")
        );
        let failing_recovery = FederationInfo {
            recovering: true,
            ..problem
        };
        assert_eq!(
            connection_banner(Some(&failing_recovery)),
            Some("This federation's data is damaged.")
        );
        assert_eq!(
            connection_banner(Some(&fed(false, true))),
            Some("Restoring your wallet from the federation. This can take a while.")
        );
    }

    #[test]
    fn backup_prompt_waits_for_funds_and_stops_once_opened() {
        assert!(!backup_banner(Amount::ZERO, false));
        assert!(backup_banner(Amount::from_sat(1), false));
        assert!(!backup_banner(Amount::from_sat(1), true));
    }

    #[test]
    fn join_page_points_at_the_invite_when_nothing_is_suggested() {
        assert!(join_description(false).contains("invite code"));
        assert!(!join_description(true).contains("invite code"));
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
            history_id: None,
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

    fn receive_info(min_deposit: Option<Amount>) -> fedimix_core::ReceiveInfo {
        fedimix_core::ReceiveInfo {
            payload: "bc1qdemo".into(),
            uri: "bitcoin:bc1qdemo".into(),
            expires_at: None,
            min_deposit,
            fee: None,
            address_repeats: false,
        }
    }

    #[test]
    fn a_fixed_minimum_says_a_deposit_at_it_is_not_credited() {
        assert_eq!(
            receive_note(&receive_info(Some(sat(10_000)))).as_deref(),
            Some("Deposits of 10000 sat or less are not credited.")
        );
        assert_eq!(receive_note(&receive_info(None)), None);
        assert_eq!(receive_note(&receive_info(Some(Amount::ZERO))), None);
    }

    #[test]
    fn a_repeating_address_says_so_and_that_its_minimum_moves() {
        let info = fedimix_core::ReceiveInfo {
            address_repeats: true,
            ..receive_info(Some(sat(2_000)))
        };
        assert_eq!(
            receive_note(&info).as_deref(),
            Some(
                "Send well above 2000 sat; this minimum follows Bitcoin fees. \
                 This address stays the same until it receives a payment."
            )
        );
        let no_minimum = fedimix_core::ReceiveInfo {
            min_deposit: None,
            ..info
        };
        assert_eq!(
            receive_note(&no_minimum).as_deref(),
            Some("This address stays the same until it receives a payment.")
        );
    }

    #[test]
    fn an_invoice_says_what_fees_take() {
        let info = fedimix_core::ReceiveInfo {
            fee: Some(sat(50)),
            ..receive_info(None)
        };
        assert_eq!(
            receive_note(&info).as_deref(),
            Some("Fees take about 50 sat of this amount.")
        );
    }

    fn rows(ids: &[&str]) -> Vec<HistoryItem> {
        ids.iter()
            .map(|id| HistoryItem {
                id: id.to_string(),
                ..hist(HistoryKind::Paid, false, HistoryStatus::Complete)
            })
            .collect()
    }

    #[test]
    fn show_more_is_offered_only_for_a_full_snapshot() {
        assert!(!history_may_have_more(0));
        assert!(!history_may_have_more(fedimix_core::SNAPSHOT_HISTORY - 1));
        assert!(history_may_have_more(fedimix_core::SNAPSHOT_HISTORY));
    }

    #[test]
    fn loaded_pages_survive_only_an_unshifted_snapshot() {
        let before = rows(&["a", "b", "c"]);
        // A status change moves nothing.
        let mut updated = rows(&["a", "b", "c"]);
        updated[0].status = HistoryStatus::Pending;
        assert!(extra_pages_line_up(&before, &updated));
        // A new row pushes the last one out: the pages below would skip it.
        assert!(!extra_pages_line_up(&before, &rows(&["n", "a", "b"])));
        assert!(!extra_pages_line_up(&before, &rows(&["a", "b"])));
    }

    #[test]
    fn rows_shown_are_the_snapshot_then_the_pages() {
        let snapshot = rows(&["a", "b"]);
        let pages = rows(&["c", "d"]);
        let ids: Vec<_> = history_to_show(&snapshot, &pages, true)
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        let mut pages = pages;
        pages[0].kind = HistoryKind::Cover;
        let ids: Vec<_> = history_to_show(&snapshot, &pages, false)
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, ["a", "b", "d"]);
    }

    #[test]
    fn operations_found_at_startup_report_their_outcome() {
        use OperationKind::*;
        let failed = OperationState::Failed {
            error: fedimix_core::FedimixError::Offline,
        };
        assert_eq!(
            resumed_outcome(Send, &OperationState::Done).as_deref(),
            Some("Payment sent")
        );
        assert_eq!(
            resumed_outcome(Refill, &OperationState::Done).as_deref(),
            Some("Refill done")
        );
        assert_eq!(
            resumed_outcome(Exit, &OperationState::Done).as_deref(),
            Some("Exit done")
        );
        assert_eq!(
            resumed_outcome(Send, &failed).as_deref(),
            Some("You are offline. Reconnect and try again")
        );
        assert_eq!(
            resumed_outcome(Send, &OperationState::Cancelled).as_deref(),
            Some("Cancelled")
        );
        assert_eq!(resumed_outcome(Send, &OperationState::Signing), None);
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
            broadcast_hold: Duration::ZERO,
            send_lock_timeout: None,
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
        assert_eq!(saving_banner(&sp, true), None);
        assert!(sp.federation.is_some());

        // Funded: caption, send review, history rows, exit now.
        let core = demo(DemoScenario::Funded);
        let events = core.subscribe();
        let sp = core.spending_state().await.unwrap();
        let caption = spending_caption(&sp);
        assert!(caption.queue_link.is_some() && !caption.shows_refill);
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
        assert!(
            caption.queue_link.unwrap().contains("for spending"),
            "overshoot joined the queue"
        );
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
            servers_shown: true,
        }
    }

    #[test]
    fn hidden_server_rows_are_not_validated_and_keep_the_value_in_force() {
        let hidden = SettingsForm {
            electrum: false,
            url: "",
            servers_shown: false,
            ..form()
        };
        let s = settings_from_form(&hidden).unwrap();
        assert_eq!(
            s.chain_backend,
            ChainBackend::Esplora { url: String::new() }
        );
        assert_eq!(s.anonymity_target, 100);
    }

    #[test]
    fn shown_server_rows_still_refuse_an_empty_url() {
        let shown = SettingsForm {
            electrum: false,
            url: " ",
            ..form()
        };
        assert_eq!(
            settings_from_form(&shown).unwrap_err(),
            "Enter a server URL"
        );
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
    fn esplora_urls_need_http_and_a_host() {
        for good in [
            "https://blockstream.info/api",
            "http://localhost:3000",
            "http://[::1]:3000/api",
            "https://mempool.space/api/",
        ] {
            assert_eq!(validate_server_url(false, good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "https",
            "https:",
            "https://",
            "https://:80",
            "ftp://example.com",
            "ssl://example.com:50002",
            "example.com",
            "https://exa mple.com",
            "https://example.com:notaport",
            "https://example.com:99999",
        ] {
            assert!(validate_server_url(false, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn electrum_urls_need_a_port() {
        for good in ["ssl://electrum.example:50002", "tcp://10.0.0.1:50001"] {
            assert_eq!(validate_server_url(true, good), Ok(()), "{good}");
        }
        for bad in [
            "ssl://electrum.example",
            "https://electrum.example:50002",
            "electrum.example:50002",
            "ssl://electrum.example:50002/path",
        ] {
            assert!(validate_server_url(true, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn proxy_is_host_and_port() {
        for good in [
            "",
            "127.0.0.1:9050",
            "localhost:9050",
            "[::1]:9050",
            "socks5://127.0.0.1:9050",
            "socks5h://tor.local:9050",
        ] {
            assert_eq!(validate_proxy(good), Ok(()), "{good}");
        }
        for bad in [
            "127.0.0.1",
            ":9050",
            "127.0.0.1:",
            "127.0.0.1:0",
            "127.0.0.1:65536",
            "http://127.0.0.1:9050",
            "exa mple:9050",
        ] {
            assert!(validate_proxy(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn form_with_a_bad_network_field_is_refused() {
        let mut f = form();
        f.url = "https://electrum.example:50002";
        assert!(settings_from_form(&f).is_err());
        let mut f = form();
        f.proxy = "127.0.0.1";
        assert!(settings_from_form(&f).is_err());
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
