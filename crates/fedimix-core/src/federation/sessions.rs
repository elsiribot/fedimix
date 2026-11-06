//! Reading what a federation's signed sessions accepted: the deposits the
//! anonymity estimate counts. Pinned by the Phase 4 Task 1 spike.

use fedimint_api_client::api::FederationApiExt as _;
use fedimint_client::ClientHandleArc;
use fedimint_core::core::DynInput;
use fedimint_core::module::ApiRequestErased;
use fedimint_core::session_outcome::{ConsensusItem, SessionStatus};
use fedimint_walletv2_common::OutputInfo;
use fedimint_walletv2_common::endpoint_constants::OUTPUT_INFO_SLICE_ENDPOINT;

use super::fedimint::{amount_of, read, sat_amount};
use crate::anonymity::{DepositSource, SessionDeposit};
use crate::error::{FedimixError, Result};

fn unreadable(session: u64, what: &str, error: impl std::fmt::Display) -> FedimixError {
    FedimixError::Internal(format!(
        "Fedimix could not read {what} of session {session} ({error})."
    ))
}

fn on_chain(value: bitcoin::Amount, outpoint: Option<bitcoin::OutPoint>) -> SessionDeposit {
    SessionDeposit {
        source: DepositSource::OnChain,
        amount: sat_amount(value),
        reference: outpoint.map(|outpoint| outpoint.to_string()),
    }
}

/// The deposit an input claims, if it is one. A v2 input names only an output
/// index, so its value is asked of the module; a failure there fails the
/// session, as skipping the input would count it as nothing.
async fn deposit_of(
    client: &ClientHandleArc,
    session: u64,
    input: &DynInput,
) -> Result<Option<SessionDeposit>> {
    use fedimint_wallet_common::WalletInput as V1;
    use fedimint_walletv2_common::WalletInput as V2;
    if let Some(input) = input.as_any().downcast_ref::<V1>() {
        return Ok(match input {
            V1::V0(proof) => Some(on_chain(proof.tx_output().value, Some(proof.outpoint()))),
            V1::V1(input) => Some(on_chain(input.tx_out.value, Some(input.outpoint))),
            _ => None,
        });
    }
    let Some(V2::V0(input_v0)) = input.as_any().downcast_ref::<V2>() else {
        return Ok(None);
    };
    let index = input_v0.output_index;
    let reply = read(
        client
            .api()
            .with_module(input.module_instance_id())
            .request_current_consensus::<Vec<OutputInfo>>(
                OUTPUT_INFO_SLICE_ENDPOINT.to_string(),
                ApiRequestErased::new((index, index + 1)),
            ),
    )
    .await?
    .map_err(|e| unreadable(session, "a deposit's value", e))?;
    let info = reply
        .into_iter()
        .find(|info| info.index == index)
        .ok_or_else(|| unreadable(session, "a deposit's value", "no such output"))?;
    if info.outpoint.is_none() {
        // Counted, but it can be neither recognised as ours nor deduplicated.
        eprintln!("fedimix: deposit output {index} of session {session} names no outpoint");
    }
    Ok(Some(on_chain(info.value, info.outpoint)))
}

/// See [`super::FederationClient::session_deposits`]. Always decodes with the
/// fallback registry: the API caches a complete session as first decoded, so
/// a fetch without it would leave every later one failing on the items of
/// modules Fedimix does not register.
pub(super) async fn session_deposits(
    client: &ClientHandleArc,
    session: u64,
) -> Result<Option<Vec<SessionDeposit>>> {
    let config = client.config().await;
    let decoders = client.decoders().clone().with_fallback();
    let version = client.core_api_version().await;
    let status = read(client.api().get_session_status(
        session,
        &decoders,
        version,
        config.global.broadcast_public_keys.as_ref(),
    ))
    .await?
    .map_err(|e| unreadable(session, "the outcome", e))?;
    let SessionStatus::Complete(outcome) = status else {
        return Ok(None);
    };
    let mut deposits = Vec::new();
    for item in &outcome.items {
        let ConsensusItem::Transaction(tx) = &item.item else {
            continue;
        };
        for input in &tx.inputs {
            if let Some(deposit) = deposit_of(client, session, input).await? {
                deposits.push(deposit);
            }
        }
        for output in &tx.outputs {
            use fedimint_lnv2_common::{LightningOutput, LightningOutputV0};
            if let Some(LightningOutput::V0(LightningOutputV0::Incoming(contract))) =
                output.as_any().downcast_ref::<LightningOutput>()
            {
                deposits.push(SessionDeposit {
                    source: DepositSource::Lightning,
                    amount: amount_of(contract.commitment.amount),
                    reference: None,
                });
            }
        }
    }
    Ok(Some(deposits))
}
