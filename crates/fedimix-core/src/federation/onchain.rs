//! Hides which wallet and mint generation a federation runs. Both are chosen
//! from the config's module kinds, per call, so nothing here keeps the client
//! alive.

use std::str::FromStr;

use bitcoin::Address;
use bitcoin::address::NetworkUnchecked;
use fedimint_client::{Client, ClientModuleInstance};
use fedimint_core::config::{ClientConfig, ClientModuleConfig};
use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, DynRawFallback};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_eventlog::{Event, EventLogId};
use fedimint_mint_client::MintClientModule as MintV1;
use fedimint_mintv2_client::MintClientModule as MintV2;
use fedimint_wallet_client::{
    DepositAddressInfo, DepositStateV2, MaxWithdrawableAmountError, WalletClientModule as WalletV1,
    WalletOperationMeta, WalletOperationMetaVariant, WithdrawState,
};
use fedimint_wallet_common::PegOutFees;
use fedimint_walletv2_client::events::{
    ReceivePaymentEvent, ReceivePaymentStatus, ReceivePaymentUpdateEvent,
};
use fedimint_walletv2_client::{
    FinalSendOperationState, ReceiveError, ReceiveMeta, SendError, SendMeta,
    WalletClientModule as WalletV2, WalletOperationMeta as WalletOperationMetaV2,
};
use futures::StreamExt;
use tokio::sync::mpsc;

use super::fedimint::{
    NO_RECORD, amount_of, failed, quote_error, read, sat_amount, submit_error, whole_sats,
};
use super::port::{DepositEvent, WatchUpdate, WithdrawQuote, WithdrawTerms};
use crate::api::{Amount, NoteBucket, SendAmount};
use crate::error::{FedimixError, Result};

const NOT_CLAIMED: &str = "The federation did not credit this deposit.";
const NO_FEERATE: &str =
    "The federation has not agreed on a Bitcoin fee rate yet. Try again in a few minutes.";
const LOG_PAGE: u64 = 100;

pub(super) fn no_on_chain() -> FedimixError {
    FedimixError::NotSetUp("This federation has no on-chain support.".into())
}

fn no_ecash() -> FedimixError {
    FedimixError::NotSetUp("This federation has no ecash module.".into())
}

/// The Bitcoin network a config names. A wallet module says it; so does LNv2,
/// for a federation with neither wallet. The config of a preview is still raw
/// bytes; that of a built client is decoded.
pub(super) fn config_network(config: &ClientConfig) -> Option<bitcoin::Network> {
    config.modules.values().find_map(|module| {
        let kind = module.kind();
        if *kind == fedimint_walletv2_common::KIND {
            typed::<fedimint_walletv2_common::config::WalletClientConfig>(module).map(|c| c.network)
        } else if *kind == fedimint_wallet_common::KIND {
            typed::<fedimint_wallet_common::config::WalletClientConfig>(module).map(|c| c.network.0)
        } else if *kind == fedimint_lnv2_common::KIND {
            typed::<fedimint_lnv2_common::config::LightningClientConfig>(module).map(|c| c.network)
        } else {
            None
        }
    })
}

fn typed<T: Decodable + Clone + 'static>(module: &ClientModuleConfig) -> Option<T> {
    match &module.config {
        DynRawFallback::Raw { raw, .. } => {
            T::consensus_decode_whole(raw, &ModuleDecoderRegistry::default()).ok()
        }
        DynRawFallback::Decoded(_) => module.cast::<T>().ok().cloned(),
    }
}

fn parse_address(address: &str, network: bitcoin::Network) -> Result<Address> {
    Address::<NetworkUnchecked>::from_str(address)
        .map_err(|_| {
            FedimixError::InvalidDestination("That is not a valid Bitcoin address.".into())
        })?
        .require_network(network)
        .map_err(|_| {
            FedimixError::InvalidDestination(
                "This address belongs to a different Bitcoin network.".into(),
            )
        })
}

/// The wallet module of whichever generation the federation runs.
pub(super) enum OnChain<'c> {
    V1(ClientModuleInstance<'c, WalletV1>),
    V2(ClientModuleInstance<'c, WalletV2>),
}

impl<'c> OnChain<'c> {
    pub(super) fn of(client: &'c Client) -> Result<Self> {
        if client
            .get_first_instance(&fedimint_walletv2_common::KIND)
            .is_some()
        {
            let module = client.get_first_module::<WalletV2>();
            return module.map(Self::V2).map_err(|_| no_on_chain());
        }
        if client
            .get_first_instance(&fedimint_wallet_common::KIND)
            .is_some()
        {
            let module = client.get_first_module::<WalletV1>();
            return module.map(Self::V1).map_err(|_| no_on_chain());
        }
        Err(no_on_chain())
    }

    pub(super) async fn min_deposit(&self) -> Result<Amount> {
        match self {
            Self::V1(w) => Ok(amount_of(w.get_fee_consensus().peg_in_abs)),
            Self::V2(w) => match read(w.receive_fee()).await? {
                Ok(fee) => Ok(sat_amount(fee)),
                Err(ReceiveError::NoConsensusFeerateAvailable) => {
                    Err(FedimixError::Internal(NO_FEERATE.into()))
                }
                Err(_) => Err(FedimixError::Offline),
            },
        }
    }

    pub(super) async fn deposit_address(&self) -> Result<(String, Option<String>)> {
        match self {
            Self::V1(w) => {
                let info = read(w.safe_allocate_deposit_address(())).await?;
                let DepositAddressInfo {
                    operation_id,
                    address,
                    ..
                } = info.map_err(|_| FedimixError::Offline)?;
                Ok((
                    address.to_string(),
                    Some(operation_id.fmt_full().to_string()),
                ))
            }
            // The scanner grinds an address from the federation's output list.
            Self::V2(w) => Ok((read(w.receive()).await?.to_string(), None)),
        }
    }

    pub(super) async fn withdraw_quote(
        &self,
        address: &str,
        amount: SendAmount,
        balance: fedimint_core::Amount,
        network: bitcoin::Network,
    ) -> Result<WithdrawQuote> {
        let parsed = parse_address(address, network)?;
        let exact = match amount {
            SendAmount::Exact(a) => Some(whole_sats(a)?),
            SendAmount::Max => None,
            SendAmount::FromInvoice => {
                return Err(FedimixError::InvalidAmount(
                    "This address does not carry an amount.".into(),
                ));
            }
        };
        let available = amount_of(balance);
        match self {
            Self::V1(w) => {
                let (send, fees) = match exact {
                    Some(send) => {
                        let fees = read(w.get_withdraw_fees(&parsed, send)).await?;
                        (send, fees.map_err(|_| FedimixError::Offline)?)
                    }
                    None => {
                        let max = read(w.max_withdrawable_amount(&parsed, balance)).await?;
                        max.map_err(|e| match e {
                            MaxWithdrawableAmountError::BalanceTooLow { .. } => {
                                too_little(available)
                            }
                            _ => FedimixError::Offline,
                        })?
                    }
                };
                let funded = send + fees.amount();
                let quote = read(w.send_fee_quote(funded))
                    .await?
                    .map_err(|e| quote_error(&e, available, sat_amount(funded)))?;
                Ok(WithdrawQuote {
                    address: address.to_string(),
                    amount: sat_amount(send),
                    fee: add(sat_amount(fees.amount()), quote.total().get_bitcoin()),
                    terms: WithdrawTerms::V1 {
                        sats_per_kvb: fees.fee_rate.sats_per_kvb,
                        total_weight: fees.total_weight,
                    },
                })
            }
            Self::V2(w) => {
                let miner = match read(w.send_fee()).await? {
                    Ok(miner) => miner,
                    Err(SendError::NoConsensusFeerateAvailable) => {
                        return Err(FedimixError::Internal(NO_FEERATE.into()));
                    }
                    Err(_) => return Err(FedimixError::Offline),
                };
                let send =
                    match exact {
                        Some(send) => send,
                        None => read(w.max_sendable_amount(balance, miner)).await?.map_err(
                            |e| match e {
                                SendError::InsufficientFunds => too_little(available),
                                _ => FedimixError::Offline,
                            },
                        )?,
                    };
                let funded = send + miner;
                let quote = read(w.send_fee_quote(funded))
                    .await?
                    .map_err(|e| quote_error(&e, available, sat_amount(funded)))?;
                Ok(WithdrawQuote {
                    address: address.to_string(),
                    amount: sat_amount(send),
                    fee: add(sat_amount(miner), quote.total().get_bitcoin()),
                    terms: WithdrawTerms::V2 {
                        miner_fee_sat: miner.to_sat(),
                    },
                })
            }
        }
    }

    /// Submits the quote; returns the operation id.
    pub(super) async fn withdraw(
        &self,
        quote: &WithdrawQuote,
        balance: fedimint_core::Amount,
        network: bitcoin::Network,
    ) -> Result<OperationId> {
        let address = parse_address(&quote.address, network)?;
        let value = whole_sats(quote.amount)?;
        let available = amount_of(balance);
        let needed = quote.amount.checked_add(quote.fee).unwrap_or(quote.amount);
        match (self, quote.terms) {
            (
                Self::V1(w),
                WithdrawTerms::V1 {
                    sats_per_kvb,
                    total_weight,
                },
            ) => {
                let fees = PegOutFees::new(sats_per_kvb, total_weight);
                w.withdraw(&address, value, fees, serde_json::json!({}))
                    .await
                    .map_err(|e| submit_error(&e, available, needed))
            }
            (Self::V2(w), WithdrawTerms::V2 { miner_fee_sat }) => {
                let miner = bitcoin::Amount::from_sat(miner_fee_sat);
                w.send(
                    address.into_unchecked(),
                    value,
                    Some(miner),
                    serde_json::json!({}),
                )
                .await
                .map_err(|e| match e {
                    SendError::InsufficientFunds => {
                        FedimixError::InsufficientFunds { available, needed }
                    }
                    SendError::DustValue => FedimixError::InvalidAmount(
                        "That amount is too small to send on chain.".into(),
                    ),
                    SendError::WrongNetwork => FedimixError::InvalidDestination(
                        "This address belongs to a different Bitcoin network.".into(),
                    ),
                    SendError::UnsupportedAddress => FedimixError::InvalidDestination(
                        "This federation cannot send to this kind of address.".into(),
                    ),
                    _ => FedimixError::Internal(
                        "The federation did not accept the withdrawal.".into(),
                    ),
                })
            }
            _ => Err(FedimixError::Internal(
                "This quote was made for a different federation.".into(),
            )),
        }
    }

    /// Reports a withdrawal's outcome: no state before the final one matters.
    pub(super) async fn follow_withdraw(
        &self,
        client: &Client,
        op: OperationId,
        tx: &mpsc::Sender<WatchUpdate>,
    ) {
        let update = match self {
            Self::V1(w) => withdraw_v1(w, client, op).await,
            Self::V2(w) => withdraw_v2(w, client, op).await,
        };
        let _ = tx.send(update).await;
    }

    pub(super) async fn follow_deposit(&self, op: OperationId, tx: &mpsc::Sender<WatchUpdate>) {
        let Self::V1(w) = self else {
            // Nothing asks for this: v2 returns no watch id.
            let _ = tx.send(failed(NO_RECORD)).await;
            return;
        };
        let mut states = match w.subscribe_deposit(op).await {
            Ok(states) => states.into_stream(),
            Err(_) => {
                let _ = tx.send(failed(NO_RECORD)).await;
                return;
            }
        };
        let required = w.get_finality_delay() + 1;
        let fee = amount_of(w.get_fee_consensus().peg_in_abs);
        while let Some(state) = states.next().await {
            let update = match state {
                DepositStateV2::WaitingForTransaction => continue,
                DepositStateV2::WaitingForConfirmation { btc_deposited, .. } => {
                    WatchUpdate::Confirming {
                        confirmations: 0,
                        required,
                        amount: sat_amount(btc_deposited),
                    }
                }
                DepositStateV2::Confirmed { btc_deposited, .. } => WatchUpdate::Confirming {
                    confirmations: required,
                    required,
                    amount: sat_amount(btc_deposited),
                },
                DepositStateV2::Claimed {
                    btc_deposited,
                    btc_out_point,
                } => WatchUpdate::Settled {
                    amount: sat_amount(btc_deposited).saturating_sub(fee),
                    fee,
                    txid: Some(btc_out_point.txid.to_string()),
                },
                DepositStateV2::Failed(_) => failed(NOT_CLAIMED),
            };
            if tx.send(update).await.is_err() {
                return;
            }
        }
    }
}

fn add(a: Amount, b: fedimint_core::Amount) -> Amount {
    a.checked_add(amount_of(b)).unwrap_or(a)
}

fn too_little(available: Amount) -> FedimixError {
    FedimixError::InvalidAmount(format!(
        "The balance of {available} cannot cover a withdrawal and its fees."
    ))
}

async fn withdraw_v1(
    w: &ClientModuleInstance<'_, WalletV1>,
    client: &Client,
    op: OperationId,
) -> WatchUpdate {
    let Ok(states) = w.subscribe_withdraw_updates(op).await else {
        return failed(NO_RECORD);
    };
    let mut states = states.into_stream();
    while let Some(state) = states.next().await {
        match state {
            WithdrawState::Created => {}
            WithdrawState::Succeeded(txid) => {
                let (amount, fee) = withdrawn_v1(client, op).await.unwrap_or_default();
                return WatchUpdate::Settled {
                    amount,
                    fee,
                    txid: Some(txid.to_string()),
                };
            }
            WithdrawState::Failed(_) => break,
        }
    }
    failed("The federation did not accept the withdrawal.")
}

async fn withdrawn_v1(client: &Client, op: OperationId) -> Option<(Amount, Amount)> {
    let entry = client.operation_log().get_operation(op).await?;
    match entry.try_meta::<WalletOperationMeta>().ok()?.variant {
        WalletOperationMetaVariant::Withdraw { amount, fee, .. } => {
            Some((sat_amount(amount), sat_amount(fee.amount())))
        }
        _ => None,
    }
}

async fn withdraw_v2(
    w: &ClientModuleInstance<'_, WalletV2>,
    client: &Client,
    op: OperationId,
) -> WatchUpdate {
    match w.await_final_send_operation_state(op).await {
        Ok(FinalSendOperationState::Success(txid)) => {
            let (amount, fee) = withdrawn_v2(client, op).await.unwrap_or_default();
            WatchUpdate::Settled {
                amount,
                fee,
                txid: Some(txid.to_string()),
            }
        }
        Ok(FinalSendOperationState::Aborted | FinalSendOperationState::Failure) => {
            failed("The federation did not accept the withdrawal.")
        }
        Err(_) => failed(NO_RECORD),
    }
}

/// Waits for the change as well: unlike v1's stream, v2 reports success while
/// the change notes are still being issued, and a settled operation must leave
/// the balance whole.
async fn withdrawn_v2(client: &Client, op: OperationId) -> Option<(Amount, Amount)> {
    let entry = client.operation_log().get_operation(op).await?;
    match entry.try_meta::<WalletOperationMetaV2>().ok()? {
        WalletOperationMetaV2::Send(SendMeta {
            change_outpoint_range,
            value,
            fee,
            ..
        }) => {
            let change = change_outpoint_range.into_iter().collect();
            let _ = client
                .await_primary_bitcoin_module_outputs(op, change)
                .await;
            Some((sat_amount(value), sat_amount(fee)))
        }
        WalletOperationMetaV2::Receive(_) => None,
    }
}

/// The v2 deposit log: each deposit gets one event as the claim starts and one
/// once its ecash exists. A claim the federation aborts is retried under a new
/// operation, so an aborted update reports nothing.
pub(super) async fn follow_deposits(client: &Client, cursor: u64, tx: &mpsc::Sender<DepositEvent>) {
    let mut added = client.log_event_added_rx();
    let mut position = EventLogId::LOG_START.saturating_add(cursor);
    loop {
        added.borrow_and_update();
        let entries = client.get_event_log(Some(position), LOG_PAGE).await;
        if entries.is_empty() {
            if added.changed().await.is_err() {
                return;
            }
            continue;
        }
        for entry in &entries {
            position = entry.id().next();
            if entry.module_kind() != Some(&fedimint_walletv2_common::KIND) {
                continue;
            }
            let cursor = u64::from(position);
            let event = if entry.kind == ReceivePaymentEvent::KIND {
                entry
                    .to_event::<ReceivePaymentEvent>()
                    .map(|e| receive_event(cursor, e))
            } else if entry.kind == ReceivePaymentUpdateEvent::KIND {
                match entry.to_event::<ReceivePaymentUpdateEvent>() {
                    Some(update) if update.status == ReceivePaymentStatus::Success => {
                        credited_event(client, cursor, update.operation_id).await
                    }
                    _ => None,
                }
            } else {
                None
            };
            if let Some(event) = event
                && tx.send(event).await.is_err()
            {
                return;
            }
        }
    }
}

fn deposit_event(
    cursor: u64,
    operation: OperationId,
    value: bitcoin::Amount,
    fee: bitcoin::Amount,
    outpoint: Option<bitcoin::OutPoint>,
    confirmed: bool,
) -> DepositEvent {
    DepositEvent {
        cursor,
        id: outpoint.map_or_else(|| operation.fmt_full().to_string(), |o| o.to_string()),
        amount: sat_amount(value).saturating_sub(sat_amount(fee)),
        fee: sat_amount(fee),
        txid: outpoint.map(|o| o.txid.to_string()),
        confirmed,
    }
}

fn receive_event(cursor: u64, e: ReceivePaymentEvent) -> DepositEvent {
    deposit_event(cursor, e.operation_id, e.value, e.fee, e.outpoint, false)
}

/// Waits until the claimed ecash is issued, as `await_receive` does, so the
/// balance holds the deposit when this event is seen. The update event carries
/// no amount, and the event with it may lie before a resumed cursor, so the
/// amount comes from the operation.
async fn credited_event(client: &Client, cursor: u64, op: OperationId) -> Option<DepositEvent> {
    let entry = client.operation_log().get_operation(op).await?;
    let WalletOperationMetaV2::Receive(ReceiveMeta {
        change_outpoint_range,
        value,
        fee,
        outpoint,
        ..
    }) = entry.try_meta::<WalletOperationMetaV2>().ok()?
    else {
        return None;
    };
    client
        .await_primary_bitcoin_module_outputs(op, change_outpoint_range.into_iter().collect())
        .await
        .ok()?;
    Some(deposit_event(cursor, op, value, fee, outpoint, true))
}

/// The mint module of whichever generation the federation runs.
pub(super) enum Mint<'c> {
    V1(ClientModuleInstance<'c, MintV1>),
    V2(ClientModuleInstance<'c, MintV2>),
}

impl<'c> Mint<'c> {
    pub(super) fn of(client: &'c Client) -> Result<Self> {
        if let Ok(mint) = client.get_first_module::<MintV2>() {
            return Ok(Self::V2(mint));
        }
        client
            .get_first_module::<MintV1>()
            .map(Self::V1)
            .map_err(|_| no_ecash())
    }

    pub(super) async fn notes(&self, client: &Client) -> Result<Vec<NoteBucket>> {
        let mut buckets: Vec<(fedimint_core::Amount, u64)> = match self {
            Self::V1(mint) => {
                let id = client
                    .get_first_instance(&fedimint_mint_client::KIND)
                    .ok_or_else(no_ecash)?;
                let mut dbtx = client.db().begin_transaction_nc().await;
                mint.get_note_counts_by_denomination(&mut dbtx.to_ref_with_prefix_module_id(id).0)
                    .await
                    .iter()
                    .map(|(amount, count)| (amount, count as u64))
                    .collect()
            }
            Self::V2(mint) => mint
                .get_count_by_denomination()
                .await
                .into_iter()
                .map(|(denomination, count)| (denomination.amount(), count))
                .collect(),
        };
        buckets.sort();
        Ok(buckets
            .into_iter()
            .map(|(denomination, count)| NoteBucket {
                denomination: amount_of(denomination),
                count: u32::try_from(count).unwrap_or(u32::MAX),
            })
            .collect())
    }
}
