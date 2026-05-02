//! Telos listener — Week 1–3 read path.
//!
//! Subscriptions:
//!   - [`watch_headers`] proves the Alloy WebSocket transport is wired up.
//!   - [`watch_intents`] applies a typed event filter for `PaymentIntent`
//!     logs emitted by a configurable Tempo contract address.
//!   - [`watch_fills`] does the same for Hyperliquid `Fill` events.
//!   - [`watch_both`] multiplexes the two streams via `tokio::select!`,
//!     which is the routing-loop primitive the solver will be built on.
//!
//! When `fork_url` is supplied, decoded intents are simulated against forked
//! state via the settler crate. The simulation is dispatched onto its own
//! task — the listener loop must stay responsive to the next event.

pub mod abi;

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use eyre::Result;
use futures_util::StreamExt;
use telos_settler::{simulate_settlement, simulate_settlement_forked};
use telos_types::PaymentIntent;
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
///
/// `fork_url` (optional): when set, each decoded intent is simulated against
/// forked state at the latest block; otherwise simulated against empty state.
pub async fn watch_intents(
    ws_url: &str,
    contract: Address,
    fork_url: Option<String>,
) -> Result<()> {
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;
    let mut stream = provider.subscribe_logs(&intent_filter(contract)).await?.into_stream();

    info!(
        target: "telos::listener",
        url = %ws_url,
        contract = %contract,
        forked = fork_url.is_some(),
        "subscribed to PaymentIntent logs",
    );

    while let Some(log) = stream.next().await {
        if let Some(intent) = decode_intent(&log) {
            spawn_simulation(intent, fork_url.clone());
        }
    }

    Ok(())
}

/// Subscribe to Hyperliquid `Fill` logs and decode each one.
pub async fn watch_fills(ws_url: &str, contract: Address) -> Result<()> {
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;
    let mut stream = provider.subscribe_logs(&fill_filter(contract)).await?.into_stream();

    info!(
        target: "telos::listener",
        url = %ws_url,
        contract = %contract,
        "subscribed to Fill logs",
    );

    while let Some(log) = stream.next().await {
        decode_fill(&log);
    }

    Ok(())
}

/// Multiplex Tempo intents and HL fills over two independent WebSocket providers.
///
/// `fork_url` is forwarded to the intent simulator. `tokio::select!` is the
/// same shape the solver loop will use — every input source becomes a stream,
/// the loop reacts to whichever fires first. Each branch is cancel-safe
/// because `StreamExt::next` is.
pub async fn watch_both(
    tempo_url: &str,
    tempo_contract: Address,
    hl_url: &str,
    hl_contract: Address,
    fork_url: Option<String>,
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
        forked = fork_url.is_some(),
        "multiplexing intent + fill streams",
    );

    loop {
        tokio::select! {
            Some(log) = intents.next() => {
                if let Some(intent) = decode_intent(&log) {
                    spawn_simulation(intent, fork_url.clone());
                }
            }
            Some(log) = fills.next() => { decode_fill(&log); }
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

fn decode_intent(log: &Log) -> Option<PaymentIntent> {
    match abi::PaymentIntent::decode_log(&log.inner) {
        Ok(decoded) => {
            let intent: PaymentIntent = decoded.data.into();
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
            Some(intent)
        }
        Err(err) => {
            warn!(target: "telos::listener", ?err, "intent decode failed");
            None
        }
    }
}

fn decode_fill(log: &Log) {
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

/// Dispatch a settler simulation onto its own task so the listener loop
/// stays responsive. Forked simulations block on RPC fetches and would
/// otherwise stall the next event.
fn spawn_simulation(intent: PaymentIntent, fork_url: Option<String>) {
    tokio::spawn(async move {
        let outcome = match fork_url {
            Some(url) => simulate_settlement_forked(&intent, url, BlockId::latest()).await,
            None => simulate_settlement(&intent),
        };
        if let Err(err) = outcome {
            warn!(target: "telos::listener", ?err, "simulation failed");
        }
    });
}
