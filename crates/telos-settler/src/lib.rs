//! Telos settler — Week 3–4 simulation harness.
//!
//! Two flavors:
//!   - [`simulate_settlement`] runs against an empty in-memory state. Cheap
//!     and synchronous, useful for proving the harness wiring is alive.
//!   - [`simulate_settlement_forked`] forks a live RPC at a chosen block via
//!     `AlloyDB`. Now exercises real account state — the simulated tx is the
//!     payer's `transfer(merchant, amount)` against the settlement asset.
//!
//! Bridging async (Alloy) to sync (REVM): `AlloyDB`'s state-fetch is async,
//! REVM's `Database` trait is sync. `WrapDatabaseAsync` calls `block_on` on
//! a tokio handle, which would deadlock from inside an async worker. We move
//! the simulation onto a `spawn_blocking` thread and pass the parent
//! runtime's `Handle` so the wrapped DB can submit fetches back to it.

mod abi;

use abi::IERC20;
use alloy::eips::BlockId;
use alloy::primitives::Bytes;
use alloy::providers::ProviderBuilder;
use alloy::sol_types::{SolCall, SolError, SolEvent};
use eyre::{Result, eyre};
use revm::{
    Context, ExecuteEvm, MainBuilder, MainContext,
    context::TxEnv,
    context::result::{ExecutionResult, Output},
    database::{AlloyDB, CacheDB, EmptyDB, WrapDatabaseAsync},
    primitives::{TxKind, U256},
};
use telos_types::PaymentIntent;
use tracing::info;

/// What the settler decides about a candidate route, before any tx is sent.
#[derive(Debug, Clone)]
pub struct SimulationOutcome {
    pub success: bool,
    pub gas_used: u64,
    /// Decoded `Error(string)` reason if the tx reverted with one.
    pub revert_reason: Option<String>,
    /// True if a matching ERC-20 `Transfer` event was emitted to the merchant.
    pub transfer_emitted: bool,
}

/// Replay the merchant settlement transfer against an empty in-memory state.
///
/// With empty state the payer has no token balance, so this will revert.
/// That's the point — the harness exercises the *failure path* end-to-end.
pub fn simulate_settlement(intent: &PaymentIntent) -> Result<SimulationOutcome> {
    let mut evm = Context::mainnet()
        .with_db(CacheDB::<EmptyDB>::default())
        .build_mainnet();

    let result = evm.transact(build_tx(intent))?.result;
    let outcome = decode_outcome(&result, intent);

    info!(
        target: "telos::settler",
        intent_id = %intent.intent_id,
        success = outcome.success,
        gas_used = outcome.gas_used,
        revert = ?outcome.revert_reason,
        transfer = outcome.transfer_emitted,
        "simulated (empty state)",
    );

    Ok(outcome)
}

/// Replay the same tx against forked state pulled from a live RPC.
pub async fn simulate_settlement_forked(
    intent: &PaymentIntent,
    rpc_url: String,
    block: BlockId,
) -> Result<SimulationOutcome> {
    let intent = intent.clone();
    let handle = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || -> Result<SimulationOutcome> {
        // Re-enter the parent runtime so WrapDatabaseAsync can find a Handle
        // and submit async state fetches to it. Without `_enter`, the blocking
        // thread has no current runtime and `WrapDatabaseAsync::new` returns None.
        let _enter = handle.enter();

        let url = rpc_url.parse()?;
        let provider = ProviderBuilder::new().connect_http(url);
        let alloy_db = AlloyDB::new(provider, block);
        let wrapped = WrapDatabaseAsync::new(alloy_db)
            .ok_or_else(|| eyre!("no tokio runtime available for AlloyDB"))?;
        let db = CacheDB::new(wrapped);

        let mut evm = Context::mainnet().with_db(db).build_mainnet();
        let result = evm.transact(build_tx(&intent))?.result;
        let outcome = decode_outcome(&result, &intent);

        info!(
            target: "telos::settler",
            intent_id = %intent.intent_id,
            success = outcome.success,
            gas_used = outcome.gas_used,
            revert = ?outcome.revert_reason,
            transfer = outcome.transfer_emitted,
            "simulated (forked)",
        );

        Ok(outcome)
    })
    .await
    .map_err(|e| eyre!("settler task panicked: {e}"))?
}

/// Build a `transfer(merchant, amount)` call against the settlement asset.
///
/// The selector + ABI-encoded args go in `data`; `kind` targets the token
/// contract (not the merchant), since ERC-20 transfers are contract calls.
fn build_tx(intent: &PaymentIntent) -> TxEnv {
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

fn decode_outcome(
    result: &ExecutionResult,
    intent: &PaymentIntent,
) -> SimulationOutcome {
    let success = result.is_success();
    let gas_used = result.tx_gas_used();

    let revert_reason = match result {
        ExecutionResult::Revert { output, .. } => decode_revert_reason(output),
        _ => None,
    };

    let transfer_emitted = match result {
        ExecutionResult::Success { output: Output::Call(_), logs, .. } => logs
            .iter()
            .any(|log| match IERC20::Transfer::decode_log(log) {
                Ok(t) => t.from == intent.payer && t.to == intent.merchant,
                Err(_) => false,
            }),
        _ => false,
    };

    SimulationOutcome { success, gas_used, revert_reason, transfer_emitted }
}

/// Decode standard `Error(string)` revert payloads.
///
/// Custom errors and `Panic(uint256)` would each need their own selector
/// match; here we only handle the plain-string case since that's what
/// OpenZeppelin's ERC-20 emits ("ERC20: transfer amount exceeds balance").
fn decode_revert_reason(output: &Bytes) -> Option<String> {
    let revert = alloy::sol_types::Revert::abi_decode(output).ok()?;
    Some(revert.reason)
}
