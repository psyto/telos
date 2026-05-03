//! Telos settler — Weeks 3–12 simulation harness.
//!
//! Two flavors:
//!   - [`simulate_settlement`] runs against an empty in-memory state. Cheap
//!     and synchronous, useful for proving the harness wiring is alive.
//!   - [`simulate_settlement_forked`] forks a live RPC at a chosen block via
//!     `AlloyDB`. Exercises real account state, balances, and bytecode.
//!
//! Both walk the same two legs in sequence: the payer's
//! `IERC20.transfer(merchant, amount)` against the settlement asset, then —
//! if a [`RouteQuote`] is supplied — `IHyperliquidGateway.placeShort(...)`
//! for the perp hedge. REVM's `Context` accumulates state between
//! `transact` calls, so the second leg sees the post-state of the first.
//! `atomic_success` is the AND of both legs (or just spot when no quote).
//!
//! Both flavors use [`telos_precompile::TelosPrecompiles`] in place of the
//! stock `EthPrecompiles`, so any tx in the simulation can call the Telos
//! `intent_digest` precompile at `0x…0901`. The leg-walking inner loop
//! [`run_legs`] is generic over `impl ExecuteEvm` so the same code drives
//! the empty-state and forked EVMs without caring what precompile provider
//! was wired in.
//!
//! Bridging async (Alloy) to sync (REVM): `AlloyDB`'s state-fetch is async,
//! REVM's `Database` trait is sync. `WrapDatabaseAsync` calls `block_on` on
//! a tokio handle, which would deadlock from inside an async worker. We move
//! the simulation onto a `spawn_blocking` thread and pass the parent
//! runtime's `Handle` so the wrapped DB can submit fetches back to it.

pub mod abi;

use abi::{IERC20, IHyperliquidGateway};
use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes};
use alloy::providers::ProviderBuilder;
use alloy::sol_types::{SolCall, SolError, SolEvent};
use eyre::{Result, eyre};
use revm::{
    Context, ExecuteEvm, MainContext,
    context::Evm,
    context::TxEnv,
    context::result::{ExecutionResult, Output},
    database::{AlloyDB, CacheDB, EmptyDB, WrapDatabaseAsync},
    handler::instructions::EthInstructions,
    primitives::{TxKind, U256},
};
use telos_precompile::TelosPrecompiles;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use telos_types::{Fill, PaymentIntent, PriceQuote, PriceSource, RouteQuote};
use tokio::sync::RwLock;
use tracing::info;

/// Per-leg result. The settler reports both legs independently so the caller
/// can see *which* leg blocked atomic settlement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegOutcome {
    pub success: bool,
    pub gas_used: u64,
    pub revert_reason: Option<String>,
}

/// Combined two-leg simulation result. `atomic_success` is the AND of both
/// legs (or just `spot` when no `RouteQuote` was supplied).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationOutcome {
    pub spot: LegOutcome,
    pub hedge: Option<LegOutcome>,
    pub transfer_emitted: bool,
    pub hedge_acked: bool,
    pub atomic_success: bool,
}

/// One transaction the submitter would broadcast on Telos's behalf.
/// Only `to`, `data`, and `gas_limit` are decided here; the submitter fills
/// nonce, gas price, and chain id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedTx {
    pub to: Address,
    pub data: Bytes,
    pub gas_limit: u64,
}

/// What the submitter receives. Telos only signs the hedge — the spot leg
/// is the merchant's settlement, initiated by the payer on Tempo, observed
/// here for *gating* but not broadcast by us. A bundler/Permit2 design
/// would change that; deferred.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmissionPlan {
    pub intent_id: alloy::primitives::B256,
    pub hedge: PreparedTx,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SettlerDecision {
    Submit(SubmissionPlan),
    Reject(RejectReason),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RejectReason {
    NoQuote,
    SpotWouldRevert(Option<String>),
    HedgeWouldRevert(Option<String>),
    PriceTooStale { age_secs: u64, max_secs: u64 },
}

#[derive(Debug, Clone, Copy)]
pub struct SubmitConfig {
    pub max_price_age_secs: u64,
}

impl Default for SubmitConfig {
    fn default() -> Self {
        Self { max_price_age_secs: 30 }
    }
}

/// Gate broadcast on the simulation outcome and quote freshness.
///
/// Reject if:
///   - no quote was produced (no price observed for the asset, or no venue);
///   - the spot leg would revert (no point hedging if settlement won't land);
///   - the hedge leg would revert (the very thing we're about to submit);
///   - the price snapshot is older than `cfg.max_price_age_secs`.
pub fn should_submit(
    outcome: &SimulationOutcome,
    route: Option<&RouteQuote>,
    intent: &PaymentIntent,
    cfg: &SubmitConfig,
) -> SettlerDecision {
    let route = match route {
        Some(r) => r,
        None => return SettlerDecision::Reject(RejectReason::NoQuote),
    };

    if !outcome.spot.success {
        return SettlerDecision::Reject(RejectReason::SpotWouldRevert(
            outcome.spot.revert_reason.clone(),
        ));
    }

    let hedge_outcome = match &outcome.hedge {
        Some(h) => h,
        None => return SettlerDecision::Reject(RejectReason::NoQuote),
    };
    if !hedge_outcome.success {
        return SettlerDecision::Reject(RejectReason::HedgeWouldRevert(
            hedge_outcome.revert_reason.clone(),
        ));
    }

    if route.price_age_secs > cfg.max_price_age_secs {
        return SettlerDecision::Reject(RejectReason::PriceTooStale {
            age_secs: route.price_age_secs,
            max_secs: cfg.max_price_age_secs,
        });
    }

    SettlerDecision::Submit(SubmissionPlan {
        intent_id: intent.intent_id,
        hedge: PreparedTx {
            to: route.hedge_venue,
            data: hedge_calldata(route, intent.max_slippage_bps),
            gas_limit: 300_000,
        },
    })
}

fn hedge_calldata(route: &RouteQuote, max_slippage_bps: u16) -> Bytes {
    Bytes::from(
        IHyperliquidGateway::placeShortCall {
            asset: route.spot_asset,
            size: route.hedge_size,
            maxSlippageBps: max_slippage_bps,
        }
        .abi_encode(),
    )
}

/// Shared, async-safe map from spot asset to its most recent observed price.
#[derive(Clone, Default)]
pub struct PriceBook {
    inner: Arc<RwLock<HashMap<Address, PriceQuote>>>,
}

impl PriceBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the book from a Hyperliquid fill — last trade is taken as the mark.
    pub async fn record_fill(&self, fill: &Fill) {
        let quote = PriceQuote {
            asset: fill.asset,
            price_e8: fill.price_e8,
            timestamp: fill.timestamp,
            source: PriceSource::HlFill,
        };
        self.inner.write().await.insert(fill.asset, quote);
    }

    pub async fn get(&self, asset: Address) -> Option<PriceQuote> {
        self.inner.read().await.get(&asset).copied()
    }
}

/// Size the spot + perp legs for an intent against the current price book.
pub async fn quote_route(
    intent: &PaymentIntent,
    prices: &PriceBook,
    hedge_venue: Address,
) -> Option<RouteQuote> {
    let quote = prices.get(intent.settlement_asset).await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();

    Some(RouteQuote {
        intent_id: intent.intent_id,
        spot_asset: intent.settlement_asset,
        spot_amount: intent.settlement_amount,
        hedge_size: intent.settlement_amount,
        hedge_venue,
        price_e8: quote.price_e8,
        price_age_secs: now.saturating_sub(quote.timestamp),
    })
}

/// Replay both legs against an empty in-memory state.
pub fn simulate_settlement(
    intent: &PaymentIntent,
    route: Option<&RouteQuote>,
    intent_max_slippage_bps: u16,
) -> Result<SimulationOutcome> {
    let ctx = Context::mainnet().with_db(CacheDB::<EmptyDB>::default());
    let spec = *ctx.cfg.spec();
    let mut evm = Evm::new(
        ctx,
        EthInstructions::new_mainnet_with_spec(spec),
        TelosPrecompiles::new(spec),
    );
    run_legs(&mut evm, intent, route, intent_max_slippage_bps, "empty state")
}

/// Replay both legs against forked state pulled from a live RPC.
pub async fn simulate_settlement_forked(
    intent: &PaymentIntent,
    route: Option<RouteQuote>,
    intent_max_slippage_bps: u16,
    rpc_url: String,
    block: BlockId,
) -> Result<SimulationOutcome> {
    let intent = intent.clone();
    let handle = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || -> Result<SimulationOutcome> {
        let _enter = handle.enter();

        let url = rpc_url.parse()?;
        let provider = ProviderBuilder::new().connect_http(url);
        let alloy_db = AlloyDB::new(provider, block);
        let wrapped = WrapDatabaseAsync::new(alloy_db)
            .ok_or_else(|| eyre!("no tokio runtime available for AlloyDB"))?;
        let db = CacheDB::new(wrapped);

        let ctx = Context::mainnet().with_db(db);
        let spec = *ctx.cfg.spec();
        let mut evm = Evm::new(
            ctx,
            EthInstructions::new_mainnet_with_spec(spec),
            TelosPrecompiles::new(spec),
        );
        run_legs(&mut evm, &intent, route.as_ref(), intent_max_slippage_bps, "forked")
    })
    .await
    .map_err(|e| eyre!("settler task panicked: {e}"))?
}

/// Walk both legs in order against whichever EVM the caller built.
///
/// Generic over `impl ExecuteEvm` so the empty-state and forked paths share
/// this code regardless of which precompile provider was wired in. The
/// `Tx = TxEnv` and `ExecutionResult = ExecutionResult` bounds pin the
/// types we actually decode against; everything else is left to inference.
/// `transact` mutates the EVM in place — the hedge tx sees any
/// nonce/balance/storage changes the spot tx made.
fn run_legs<E>(
    evm: &mut E,
    intent: &PaymentIntent,
    route: Option<&RouteQuote>,
    intent_max_slippage_bps: u16,
    label: &str,
) -> Result<SimulationOutcome>
where
    E: ExecuteEvm<Tx = TxEnv, ExecutionResult = ExecutionResult>,
    E::Error: std::error::Error + Send + Sync + 'static,
{
    let spot_result = evm.transact(build_spot_tx(intent))?.result;
    let spot = decode_leg(&spot_result);
    let transfer_emitted = scan_transfer_event(&spot_result, intent.payer, intent.merchant);

    let (hedge, hedge_acked) = match route {
        Some(r) => {
            let hedge_result = evm
                .transact(build_hedge_tx(intent.payer, r, intent_max_slippage_bps))?
                .result;
            let acked = scan_order_placed_event(&hedge_result, r.spot_asset);
            (Some(decode_leg(&hedge_result)), acked)
        }
        None => (None, false),
    };

    let atomic_success = spot.success && hedge.as_ref().map(|h| h.success).unwrap_or(true);

    let outcome = SimulationOutcome {
        spot,
        hedge,
        transfer_emitted,
        hedge_acked,
        atomic_success,
    };

    info!(
        target: "telos::settler",
        intent_id = %intent.intent_id,
        atomic = outcome.atomic_success,
        spot_success = outcome.spot.success,
        spot_revert = ?outcome.spot.revert_reason,
        spot_gas = outcome.spot.gas_used,
        transfer = outcome.transfer_emitted,
        hedge_success = ?outcome.hedge.as_ref().map(|h| h.success),
        hedge_revert = ?outcome.hedge.as_ref().and_then(|h| h.revert_reason.clone()),
        hedge_gas = ?outcome.hedge.as_ref().map(|h| h.gas_used),
        hedge_acked = outcome.hedge_acked,
        mode = label,
        "simulated",
    );

    Ok(outcome)
}

fn build_spot_tx(intent: &PaymentIntent) -> TxEnv {
    let calldata = IERC20::transferCall {
        to: intent.merchant,
        amount: intent.settlement_amount,
    }
    .abi_encode();

    TxEnv {
        caller: intent.payer,
        kind: TxKind::Call(intent.settlement_asset),
        value: U256::ZERO,
        data: Bytes::from(calldata),
        gas_limit: 200_000,
        gas_price: 0,
        ..Default::default()
    }
}

fn build_hedge_tx(payer: Address, route: &RouteQuote, max_slippage_bps: u16) -> TxEnv {
    let calldata = IHyperliquidGateway::placeShortCall {
        asset: route.spot_asset,
        size: route.hedge_size,
        maxSlippageBps: max_slippage_bps,
    }
    .abi_encode();

    TxEnv {
        caller: payer,
        kind: TxKind::Call(route.hedge_venue),
        value: U256::ZERO,
        data: Bytes::from(calldata),
        gas_limit: 300_000,
        gas_price: 0,
        ..Default::default()
    }
}

fn decode_leg(result: &ExecutionResult) -> LegOutcome {
    LegOutcome {
        success: result.is_success(),
        gas_used: result.tx_gas_used(),
        revert_reason: match result {
            ExecutionResult::Revert { output, .. } => decode_revert_reason(output),
            _ => None,
        },
    }
}

fn scan_transfer_event(result: &ExecutionResult, from: Address, to: Address) -> bool {
    match result {
        ExecutionResult::Success { output: Output::Call(_), logs, .. } => {
            logs.iter().any(|log| match IERC20::Transfer::decode_log(log) {
                Ok(t) => t.from == from && t.to == to,
                Err(_) => false,
            })
        }
        _ => false,
    }
}

fn scan_order_placed_event(result: &ExecutionResult, asset: Address) -> bool {
    match result {
        ExecutionResult::Success { logs, .. } => {
            logs.iter().any(|log| match IHyperliquidGateway::OrderPlaced::decode_log(log) {
                Ok(o) => o.asset == asset,
                Err(_) => false,
            })
        }
        _ => false,
    }
}

/// Decode standard `Error(string)` revert payloads.
fn decode_revert_reason(output: &Bytes) -> Option<String> {
    let revert = alloy::sol_types::Revert::abi_decode(output).ok()?;
    Some(revert.reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use telos_precompile::{INTENT_DIGEST_INPUT_LEN, TELOS_INTENT_DIGEST_ADDRESS};

    /// The custom precompile is reachable from inside the settler's REVM.
    /// A bundler/router contract on Tempo could call `intent_digest` mid-tx
    /// to commit the canonical intent identity; this test proves the
    /// precompile is wired into every simulation, not just the standalone
    /// tests in the precompile crate.
    #[test]
    fn settler_evm_can_call_intent_digest_precompile() {
        let ctx = Context::mainnet().with_db(CacheDB::<EmptyDB>::default());
        let spec = *ctx.cfg.spec();
        let mut evm = Evm::new(
            ctx,
            EthInstructions::new_mainnet_with_spec(spec),
            TelosPrecompiles::new(spec),
        );

        let calldata = vec![0xAB; INTENT_DIGEST_INPUT_LEN];
        let tx = TxEnv {
            caller: Address::ZERO,
            kind: TxKind::Call(TELOS_INTENT_DIGEST_ADDRESS),
            value: U256::ZERO,
            data: Bytes::from(calldata.clone()),
            gas_limit: 100_000,
            gas_price: 0,
            ..Default::default()
        };

        let result = evm.transact(tx).expect("transact succeeds").result;
        let output = match result {
            ExecutionResult::Success { output: Output::Call(b), .. } => b,
            other => panic!("expected Success, got {other:?}"),
        };
        let expected = alloy::primitives::keccak256(calldata.as_slice());
        assert_eq!(output.as_ref(), expected.as_slice());
    }
}
