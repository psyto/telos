//! Telos listener — Week 1–5 read path.
//!
//! Subscriptions:
//!   - [`watch_headers`] proves the Alloy WebSocket transport is wired up.
//!   - [`watch_intents`] applies a typed event filter for `PaymentIntent`
//!     logs emitted by a configurable Tempo contract address.
//!   - [`watch_fills`] does the same for Hyperliquid `Fill` events; each fill
//!     updates the shared [`PriceBook`].
//!   - [`watch_both`] multiplexes the two streams via `tokio::select!`,
//!     which is the routing-loop primitive the solver will be built on.
//!
//! Feedback loop: HL fills mark prices into the book; the next decoded intent
//! reads the book to size its hedge. The simulation runs on its own task so
//! the listener loop stays responsive.

pub mod abi;

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use eyre::Result;
use futures_util::StreamExt;
use telos_settler::{
    PriceBook, SettlerDecision, SubmitConfig, quote_route, should_submit, simulate_settlement,
    simulate_settlement_forked,
};
use telos_submitter::{ConfirmConfig, Submitter};
use telos_types::{Fill, PaymentIntent};
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
pub async fn watch_intents(
    ws_url: &str,
    contract: Address,
    prices: PriceBook,
    hedge_venue: Option<Address>,
    fork_url: Option<String>,
    submitter: Option<Submitter>,
) -> Result<()> {
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;
    let mut stream = provider.subscribe_logs(&intent_filter(contract)).await?.into_stream();

    info!(
        target: "telos::listener",
        url = %ws_url,
        contract = %contract,
        forked = fork_url.is_some(),
        hedging = hedge_venue.is_some(),
        submitting = submitter.is_some(),
        "subscribed to PaymentIntent logs",
    );

    while let Some(log) = stream.next().await {
        if let Some(intent) = decode_intent(&log) {
            spawn_simulation(
                intent,
                prices.clone(),
                hedge_venue,
                fork_url.clone(),
                submitter.clone(),
            );
        }
    }

    Ok(())
}

/// Subscribe to Hyperliquid `Fill` logs; each fill updates the price book.
pub async fn watch_fills(ws_url: &str, contract: Address, prices: PriceBook) -> Result<()> {
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;
    let mut stream = provider.subscribe_logs(&fill_filter(contract)).await?.into_stream();

    info!(
        target: "telos::listener",
        url = %ws_url,
        contract = %contract,
        "subscribed to Fill logs",
    );

    while let Some(log) = stream.next().await {
        if let Some(fill) = decode_fill(&log) {
            prices.record_fill(&fill).await;
        }
    }

    Ok(())
}

/// Multiplex Tempo intents and HL fills over two independent WebSocket providers.
#[allow(clippy::too_many_arguments)]
pub async fn watch_both(
    tempo_url: &str,
    tempo_contract: Address,
    hl_url: &str,
    hl_contract: Address,
    prices: PriceBook,
    hedge_venue: Option<Address>,
    fork_url: Option<String>,
    submitter: Option<Submitter>,
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
        hedging = hedge_venue.is_some(),
        submitting = submitter.is_some(),
        "multiplexing intent + fill streams",
    );

    loop {
        tokio::select! {
            Some(log) = intents.next() => {
                if let Some(intent) = decode_intent(&log) {
                    spawn_simulation(
                        intent,
                        prices.clone(),
                        hedge_venue,
                        fork_url.clone(),
                        submitter.clone(),
                    );
                }
            }
            Some(log) = fills.next() => {
                if let Some(fill) = decode_fill(&log) {
                    prices.record_fill(&fill).await;
                }
            }
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

fn decode_fill(log: &Log) -> Option<Fill> {
    match abi::Fill::decode_log(&log.inner) {
        Ok(decoded) => {
            let fill: Fill = decoded.data.into();
            info!(
                target: "telos::listener",
                trader = %fill.trader,
                order_id = %fill.order_id,
                asset = %fill.asset,
                is_buy = fill.is_buy,
                price_e8 = fill.price_e8,
                size_e8 = fill.size_e8,
                timestamp = fill.timestamp,
                "fill",
            );
            Some(fill)
        }
        Err(err) => {
            warn!(target: "telos::listener", ?err, "fill decode failed");
            None
        }
    }
}

/// Quote the route, dispatch the settler simulation, gate on the outcome,
/// and hand the plan to the submitter when approved. All on a spawned task
/// so the listener loop stays responsive — forked sims block on RPC fetches.
fn spawn_simulation(
    intent: PaymentIntent,
    prices: PriceBook,
    hedge_venue: Option<Address>,
    fork_url: Option<String>,
    submitter: Option<Submitter>,
) {
    tokio::spawn(async move {
        let route = match hedge_venue {
            Some(venue) => match quote_route(&intent, &prices, venue).await {
                Some(r) => {
                    info!(
                        target: "telos::listener",
                        intent_id = %r.intent_id,
                        spot_amount = %r.spot_amount,
                        hedge_size = %r.hedge_size,
                        hedge_venue = %r.hedge_venue,
                        price_e8 = r.price_e8,
                        price_age_secs = r.price_age_secs,
                        "quoted",
                    );
                    Some(r)
                }
                None => {
                    warn!(
                        target: "telos::listener",
                        intent_id = %intent.intent_id,
                        asset = %intent.settlement_asset,
                        "no price for asset — running spot-only simulation",
                    );
                    None
                }
            },
            None => None,
        };

        let outcome = match fork_url {
            Some(url) => {
                simulate_settlement_forked(
                    &intent,
                    route.clone(),
                    intent.max_slippage_bps,
                    url,
                    BlockId::latest(),
                )
                .await
            }
            None => simulate_settlement(&intent, route.as_ref(), intent.max_slippage_bps),
        };

        let outcome = match outcome {
            Ok(o) => o,
            Err(err) => {
                warn!(target: "telos::listener", ?err, "simulation failed");
                return;
            }
        };

        let decision = should_submit(&outcome, route.as_ref(), &intent, &SubmitConfig::default());
        match decision {
            SettlerDecision::Submit(plan) => match submitter.as_ref() {
                Some(s) => match s.submit_and_confirm(&plan, ConfirmConfig::default()).await {
                    Ok(result) => info!(
                        target: "telos::listener",
                        intent_id = %intent.intent_id,
                        ?result,
                        "settled",
                    ),
                    Err(err) => warn!(
                        target: "telos::listener",
                        ?err,
                        intent_id = %intent.intent_id,
                        "submit failed",
                    ),
                },
                None => info!(
                    target: "telos::listener",
                    intent_id = %plan.intent_id,
                    hedge_to = %plan.hedge.to,
                    "approved but no submitter configured",
                ),
            },
            SettlerDecision::Reject(reason) => info!(
                target: "telos::listener",
                intent_id = %intent.intent_id,
                ?reason,
                "rejected",
            ),
        }
    });
}
