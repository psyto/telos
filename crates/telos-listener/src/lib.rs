//! Telos listener — Week 1–2 read path.
//!
//! Three flavors of subscription:
//!   - [`watch_headers`] proves the Alloy WebSocket transport is wired up.
//!   - [`watch_intents`] applies a typed event filter for `PaymentIntent`
//!     logs emitted by a configurable Tempo contract address.
//!   - [`watch_fills`] does the same for Hyperliquid `Fill` events.
//!   - [`watch_both`] multiplexes the two streams via `tokio::select!`,
//!     which is the routing-loop primitive the solver will be built on.
//!
//! No execution, no decisions yet. Routing and hedging come in Weeks 3+.

pub mod abi;

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::{Filter, Log};
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

/// Subscribe to Tempo `PaymentIntent` logs and decode each one.
pub async fn watch_intents(ws_url: &str, contract: Address) -> Result<()> {
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;
    let filter = intent_filter(contract);
    let mut stream = provider.subscribe_logs(&filter).await?.into_stream();

    info!(
        target: "telos::listener",
        url = %ws_url,
        contract = %contract,
        "subscribed to PaymentIntent logs",
    );

    while let Some(log) = stream.next().await {
        handle_intent_log(&log);
    }

    Ok(())
}

/// Subscribe to Hyperliquid `Fill` logs and decode each one.
pub async fn watch_fills(ws_url: &str, contract: Address) -> Result<()> {
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;
    let filter = fill_filter(contract);
    let mut stream = provider.subscribe_logs(&filter).await?.into_stream();

    info!(
        target: "telos::listener",
        url = %ws_url,
        contract = %contract,
        "subscribed to Fill logs",
    );

    while let Some(log) = stream.next().await {
        handle_fill_log(&log);
    }

    Ok(())
}

/// Multiplex Tempo intents and HL fills over two independent WebSocket providers.
///
/// The `tokio::select!` loop is the same shape the solver will eventually use:
/// every input source becomes a stream, and the loop reacts to whichever fires
/// first. Each branch must be cancel-safe — `StreamExt::next` is.
pub async fn watch_both(
    tempo_url: &str,
    tempo_contract: Address,
    hl_url: &str,
    hl_contract: Address,
) -> Result<()> {
    let tempo = ProviderBuilder::new().connect_ws(WsConnect::new(tempo_url)).await?;
    let hl = ProviderBuilder::new().connect_ws(WsConnect::new(hl_url)).await?;

    let mut intents = tempo
        .subscribe_logs(&intent_filter(tempo_contract))
        .await?
        .into_stream();
    let mut fills = hl
        .subscribe_logs(&fill_filter(hl_contract))
        .await?
        .into_stream();

    info!(
        target: "telos::listener",
        tempo_url = %tempo_url,
        tempo_contract = %tempo_contract,
        hl_url = %hl_url,
        hl_contract = %hl_contract,
        "multiplexing intent + fill streams",
    );

    loop {
        tokio::select! {
            Some(log) = intents.next() => handle_intent_log(&log),
            Some(log) = fills.next() => handle_fill_log(&log),
            else => break,
        }
    }

    Ok(())
}

fn intent_filter(contract: Address) -> Filter {
    Filter::new()
        .address(contract)
        .event_signature(abi::PaymentIntent::SIGNATURE_HASH)
}

fn fill_filter(contract: Address) -> Filter {
    Filter::new()
        .address(contract)
        .event_signature(abi::Fill::SIGNATURE_HASH)
}

fn handle_intent_log(log: &Log) {
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
            // Run the simulation harness inline. Once the settler grows beyond
            // microseconds we move it onto a dedicated worker, but at this
            // stage the read loop is the right place — failures are visible.
            if let Err(err) = telos_settler::simulate_settlement(&intent) {
                warn!(target: "telos::listener", ?err, "simulation failed");
            }
        }
        Err(err) => warn!(target: "telos::listener", ?err, "intent decode failed"),
    }
}

fn handle_fill_log(log: &Log) {
    match abi::Fill::decode_log(&log.inner) {
        Ok(decoded) => {
            let fill: telos_types::Fill = decoded.data.into();
            info!(
                target: "telos::listener",
                trader = %fill.trader,
                order_id = %fill.order_id,
                is_buy = fill.is_buy,
                price_e8 = fill.price_e8,
                size_e8 = fill.size_e8,
                timestamp = fill.timestamp,
                "fill",
            );
        }
        Err(err) => warn!(target: "telos::listener", ?err, "fill decode failed"),
    }
}
