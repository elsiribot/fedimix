//! Static mockup data for the demo scenarios.

use super::DemoScenario;
use crate::api::*;

/// Fixed "now" so demo data is deterministic.
pub const BASE_TS: Timestamp = 1_793_318_400;
const HOUR: u64 = 3_600;
const DAY: u64 = 86_400;

pub const FEDERATION_NAME: &str = "Fedi Alpha";
pub const FEDERATION_ID: &str = "demo-fed-alpha";
pub const DEVICE_NAME: &str = "Ledger Nano S+";
pub const DEVICE_ID: &str = "demo-ledger-0";
/// Wraps only where a line must break: no spaces for most of its length.
const LONG_FEDERATION: &str = "The-Extraordinarily-Long-Named-Community-Mutual-Aid-Federation \
of the Greater Metropolitan Area and All of Its Surrounding Districts";
const LONG_DEVICE: &str = "Ledger Nano S Plus Limited Anniversary Edition Hardware Wallet \
With An Unreasonably Long Display Name";

pub fn btc(v: f64) -> Amount {
    Amount::from_sat((v * 1e8).round() as u64)
}

pub struct Fixture {
    pub device: Option<DeviceInfo>,
    /// What `detect_devices` finds.
    pub detected: Vec<DeviceInfo>,
    pub suggested: Vec<SuggestedFederation>,
    pub in_flight: Amount,
    pub coins: Vec<Coin>,
    pub saving_history: Vec<HistoryItem>,
    pub federation: Option<FederationInfo>,
    pub spending_balance: Amount,
    pub for_spending: Amount,
    pub moving: Amount,
    pub saving_ready: bool,
    pub spending_history: Vec<HistoryItem>,
    pub queue: Vec<QueueItem>,
    pub waiting_for_saving: bool,
}

pub fn device() -> DeviceInfo {
    DeviceInfo {
        id: DEVICE_ID.to_string(),
        name: DEVICE_NAME.to_string(),
        kind: DeviceKind::Ledger,
        connected: true,
    }
}

pub fn federation(online: bool) -> FederationInfo {
    FederationInfo {
        id: FEDERATION_ID.to_string(),
        name: FEDERATION_NAME.to_string(),
        online,
        anonymity_estimate: 60,
    }
}

pub fn address_for(n: usize) -> String {
    format!("bc1qdemo{n:038}")
}

pub fn outpoint_for(n: usize) -> String {
    format!("{n:064x}:0")
}

fn coin(n: usize, amount: f64, tag: PrivacyTag, age_secs: u64) -> Coin {
    let from_spending = matches!(
        tag,
        PrivacyTag::Anonymity { .. } | PrivacyTag::EarlyExit { .. }
    );
    Coin {
        outpoint: outpoint_for(n),
        amount: btc(amount),
        tag,
        received_at: BASE_TS - age_secs,
        address: address_for(n),
        source_federation: from_spending.then(|| FEDERATION_NAME.to_string()),
        frozen: false,
    }
}

fn hist(
    id: &str,
    kind: HistoryKind,
    amount: f64,
    incoming: bool,
    status: HistoryStatus,
    age_secs: u64,
) -> HistoryItem {
    HistoryItem {
        id: id.to_string(),
        kind,
        amount: btc(amount),
        incoming,
        status,
        timestamp: BASE_TS - age_secs,
    }
}

fn complete() -> HistoryStatus {
    HistoryStatus::Complete
}

fn funded_coins() -> Vec<Coin> {
    vec![
        coin(1, 0.1, PrivacyTag::Anonymity { estimate: 64 }, 2 * HOUR),
        coin(2, 0.02, PrivacyTag::Anonymity { estimate: 71 }, 20 * HOUR),
        coin(3, 0.1, PrivacyTag::Anonymity { estimate: 52 }, 3 * DAY),
        coin(4, 0.02, PrivacyTag::EarlyExit { estimate: 12 }, 4 * DAY),
        coin(5, 0.01, PrivacyTag::Anonymity { estimate: 88 }, 6 * DAY),
        coin(6, 0.9534, PrivacyTag::Unmixed, 10 * DAY),
    ]
}

fn funded_saving_history() -> Vec<HistoryItem> {
    vec![
        hist(
            "s-h1",
            HistoryKind::FromSpending {
                denomination: btc(0.1),
            },
            0.1,
            true,
            HistoryStatus::Confirming {
                confirmations: 2,
                required: 3,
            },
            2 * HOUR,
        ),
        hist(
            "s-h2",
            HistoryKind::FromSpending {
                denomination: btc(0.02),
            },
            0.02,
            true,
            complete(),
            20 * HOUR,
        ),
        hist(
            "s-h3",
            HistoryKind::Sent {
                change: Some(SentChange {
                    amount: btc(0.0313),
                    to_spending: true,
                }),
            },
            0.25,
            false,
            complete(),
            DAY + 2 * HOUR,
        ),
        hist(
            "s-h4",
            HistoryKind::Received { unmixed: true },
            0.9534,
            true,
            complete(),
            10 * DAY,
        ),
    ]
}

fn funded_spending_history() -> Vec<HistoryItem> {
    vec![
        hist(
            "p-h1",
            HistoryKind::Paid,
            0.0012,
            false,
            complete(),
            HOUR / 2,
        ),
        hist(
            "p-h2",
            HistoryKind::ToSaving {
                denomination: btc(0.1),
            },
            0.1,
            false,
            complete(),
            2 * HOUR,
        ),
        hist(
            "p-c1",
            HistoryKind::Cover,
            0.000_002,
            false,
            complete(),
            3 * HOUR,
        ),
        hist(
            "p-h3",
            HistoryKind::Deposit,
            0.05,
            true,
            HistoryStatus::Confirming {
                confirmations: 1,
                required: 3,
            },
            4 * HOUR,
        ),
        hist(
            "p-c2",
            HistoryKind::Cover,
            0.000_002,
            false,
            complete(),
            5 * HOUR,
        ),
        hist(
            "p-h4",
            HistoryKind::Deposit,
            0.4,
            true,
            complete(),
            DAY + 4 * HOUR,
        ),
    ]
}

fn queue_item(n: usize, denom: f64, status: QueueItemStatus, cur: u32, att: u32) -> QueueItem {
    QueueItem {
        id: format!("queue-{n}"),
        denomination: btc(denom),
        status,
        anonymity_current: cur,
        anonymity_attainable: att,
    }
}

fn funded_queue() -> Vec<QueueItem> {
    vec![
        queue_item(1, 0.2, QueueItemStatus::Queued, 35, 60),
        queue_item(2, 0.1, QueueItemStatus::Queued, 35, 60),
        queue_item(3, 0.1, QueueItemStatus::Queued, 12, 48),
        queue_item(4, 0.02, QueueItemStatus::Eligible, 71, 71),
    ]
}

fn deferred(reason: DeferReason) -> QueueItemStatus {
    QueueItemStatus::Deferred { reason }
}

/// Spending cannot pay exit fees; every deferral and unusual history status.
fn exit_deferred() -> Fixture {
    let moving = 0.1;
    let for_spending = 0.000_002;
    Fixture {
        spending_balance: btc(moving + for_spending),
        for_spending: btc(for_spending),
        moving: btc(moving),
        queue: vec![
            queue_item(1, 0.05, deferred(DeferReason::FeeUnaffordable), 40, 60),
            queue_item(2, 0.02, deferred(DeferReason::FeeGuard), 35, 60),
            queue_item(3, 0.02, deferred(DeferReason::GracePeriod), 12, 48),
            queue_item(4, 0.005, deferred(DeferReason::Offline), 12, 48),
            queue_item(
                5,
                0.002,
                QueueItemStatus::Withdrawing {
                    txid: outpoint_for(50),
                },
                71,
                71,
            ),
            queue_item(6, 0.002, QueueItemStatus::Landed, 71, 71),
            queue_item(7, 0.001, QueueItemStatus::Scheduled { at: BASE_TS }, 71, 71),
        ],
        spending_history: vec![
            hist(
                "p-p",
                HistoryKind::Paid,
                0.001,
                false,
                HistoryStatus::Pending,
                HOUR,
            ),
            hist(
                "p-f",
                HistoryKind::Paid,
                0.002,
                false,
                HistoryStatus::Failed,
                2 * HOUR,
            ),
            hist(
                "p-r",
                HistoryKind::Paid,
                0.003,
                false,
                HistoryStatus::Refunded,
                3 * HOUR,
            ),
            hist(
                "p-e",
                HistoryKind::ReceivedLightning,
                0.004,
                true,
                HistoryStatus::Expired,
                4 * HOUR,
            ),
        ],
        ..funded()
    }
}

fn long_names() -> Fixture {
    let device = DeviceInfo {
        name: LONG_DEVICE.to_string(),
        ..device()
    };
    let coins = funded_coins()
        .into_iter()
        .map(|c| Coin {
            source_federation: c.source_federation.map(|_| LONG_FEDERATION.to_string()),
            ..c
        })
        .collect();
    Fixture {
        device: Some(device.clone()),
        detected: vec![device],
        suggested: vec![SuggestedFederation {
            name: LONG_FEDERATION.to_string(),
            invite: "fed1demolong".to_string(),
        }],
        coins,
        federation: Some(FederationInfo {
            name: LONG_FEDERATION.to_string(),
            ..federation(true)
        }),
        spending_balance: btc(0.43),
        for_spending: btc(0.01),
        ..funded()
    }
}

fn funded() -> Fixture {
    Fixture {
        device: Some(device()),
        detected: vec![device()],
        suggested: suggested(),
        in_flight: btc(0.42),
        coins: funded_coins(),
        saving_history: funded_saving_history(),
        federation: Some(federation(true)),
        spending_balance: btc(0.47),
        for_spending: btc(0.05),
        moving: btc(0.42),
        saving_ready: true,
        spending_history: funded_spending_history(),
        queue: funded_queue(),
        waiting_for_saving: false,
    }
}

pub fn fixture(scenario: DemoScenario) -> Fixture {
    match scenario {
        DemoScenario::Funded
        | DemoScenario::PreviewExpiry
        | DemoScenario::DeviceRejects
        | DemoScenario::SendStuck => funded(),
        DemoScenario::BelowTarget => {
            let history = funded_saving_history();
            Fixture {
                in_flight: Amount::ZERO,
                coins: vec![
                    coin(1, 0.01, PrivacyTag::Anonymity { estimate: 88 }, 6 * DAY),
                    coin(2, 0.05, PrivacyTag::EarlyExit { estimate: 9 }, 4 * DAY),
                    coin(3, 0.1, PrivacyTag::Anonymity { estimate: 64 }, 2 * DAY),
                    coin(4, 0.9534, PrivacyTag::Unmixed, 10 * DAY),
                ],
                saving_history: history
                    .into_iter()
                    .filter(|h| matches!(h.id.as_str(), "s-h3" | "s-h4"))
                    .collect(),
                spending_balance: btc(0.0112),
                for_spending: btc(0.0112),
                moving: Amount::ZERO,
                spending_history: vec![hist(
                    "p-h1",
                    HistoryKind::Paid,
                    0.0012,
                    false,
                    complete(),
                    HOUR / 2,
                )],
                queue: Vec::new(),
                ..funded()
            }
        }
        DemoScenario::NoSaving => Fixture {
            device: None,
            in_flight: Amount::ZERO,
            coins: Vec::new(),
            saving_history: Vec::new(),
            saving_ready: false,
            spending_history: vec![hist(
                "p-h4",
                HistoryKind::Deposit,
                0.4,
                true,
                complete(),
                DAY + 4 * HOUR,
            )],
            waiting_for_saving: true,
            ..funded()
        },
        DemoScenario::FirstRun => Fixture {
            device: None,
            detected: vec![device()],
            suggested: suggested(),
            in_flight: Amount::ZERO,
            coins: Vec::new(),
            saving_history: Vec::new(),
            federation: None,
            spending_balance: Amount::ZERO,
            for_spending: Amount::ZERO,
            moving: Amount::ZERO,
            saving_ready: false,
            spending_history: Vec::new(),
            queue: Vec::new(),
            waiting_for_saving: false,
        },
        DemoScenario::Offline => Fixture {
            federation: Some(federation(false)),
            queue: funded_queue()
                .into_iter()
                .map(|item| QueueItem {
                    status: deferred(DeferReason::Offline),
                    ..item
                })
                .collect(),
            ..funded()
        },
        DemoScenario::ExitDeferred => exit_deferred(),
        DemoScenario::LongNames => long_names(),
        DemoScenario::DeviceDisconnected => Fixture {
            device: Some(DeviceInfo {
                connected: false,
                ..device()
            }),
            detected: Vec::new(),
            ..funded()
        },
    }
}

pub fn suggested() -> Vec<SuggestedFederation> {
    vec![
        SuggestedFederation {
            name: FEDERATION_NAME.to_string(),
            invite: "fed1demoalpha".to_string(),
        },
        SuggestedFederation {
            name: "Fedi Beta".to_string(),
            invite: "fed1demobeta".to_string(),
        },
    ]
}
