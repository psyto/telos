//! Telos listener — Week 1 read path.
//!
//! Two flavors of subscription:
//!   - [`watch_headers`] proves the Alloy WebSocket transport is wired up.
//!   - [`watch_intents`] applies a typed event filter for `PaymentIntent`
//!     logs emitted by a configurable contract address.
//!
//! No execution, no decisions yet. Routing and hedging come in Weeks 3+.

pub mod abi;

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use eyre::Result;
use futures_util::StreamExt;
use tracing::{info, warn};

/// Subscribe to new block headers and log each one.
pub async fn watch_headers(ws_url: &str) -> Result<()> {
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;

    let mut stream = provider.subscribe_blocks().await?.into_stream();

    info!(target: "telos::listener", url = %ws_url, "subscribed to block headers");

    while let Some(header) = stream.next().await {
        info!(
            target: "telos::listener",
            number = header.number,
            hash = %header.hash,
            gas_used = header.gas_used,
            "block",
        );
    }

    Ok(())
}

/// Subscribe to `PaymentIntent` logs emitted by `contract` and decode each
/// one into the typed [`telos_types::PaymentIntent`].
pub async fn watch_intents(ws_url: &str, contract: Address) -> Result<()> {
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;

    let filter = Filter::new()
        .address(contract)
        .event_signature(abi::PaymentIntent::SIGNATURE_HASH);

    let mut stream = provider.subscribe_logs(&filter).await?.into_stream();

    info!(
        target: "telos::listener",
        url = %ws_url,
        contract = %contract,
        topic = %abi::PaymentIntent::SIGNATURE_HASH,
        "subscribed to PaymentIntent logs",
    );

    while let Some(log) = stream.next().await {
        match abi::PaymentIntent::decode_log(&log.inner) {
            Ok(decoded) => {
                let intent: telos_types::PaymentIntent = decoded.data.into();
                info!(
                    target: "telos::listener",
                    intent_id = %intent.intent_id,
                    payer = %intent.payer,
                    merchant = %intent.merchant,
                    asset = %intent.settlement_asset,
                    amount = %intent.settlement_amount,
                    slippage_bps = intent.max_slippage_bps,
                    deadline = intent.deadline,
                    "intent",
                );
            }
            Err(err) => warn!(target: "telos::listener", ?err, "decode failed"),
        }
    }

    Ok(())
}
