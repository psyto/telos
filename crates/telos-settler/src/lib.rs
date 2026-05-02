//! Telos settler — Week 3 simulation harness.
//!
//! Two flavors:
//!   - [`simulate_settlement`] runs against an empty in-memory state. Cheap
//!     and synchronous, useful for proving the harness wiring is alive.
//!   - [`simulate_settlement_forked`] forks a live RPC at a chosen block via
//!     `AlloyDB`. The harness now exercises real account state, balances,
//!     and bytecode — the next step is encoding actual swap calldata.
//!
//! Bridging async (Alloy) to sync (REVM): `AlloyDB`'s state-fetch is async,
//! REVM's `Database` trait is sync. `WrapDatabaseAsync` calls `block_on` on
//! a tokio handle, which would deadlock from inside an async worker. We move
//! the simulation onto a `spawn_blocking` thread and pass the parent
//! runtime's `Handle` so the wrapped DB can submit fetches back to it.

use alloy::eips::BlockId;
use alloy::providers::ProviderBuilder;
use eyre::{Result, eyre};
use revm::{
    Context, ExecuteEvm, MainBuilder, MainContext,
    context::TxEnv,
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
}

/// Replay the merchant settlement transfer against an empty in-memory state.
///
/// This is the harness, not the route — the actual swap+hedge calldata gets
/// added once the precompile and venue ABIs are in place. What this proves
/// is that REVM is correctly wired and reachable from the listener loop.
pub fn simulate_settlement(intent: &PaymentIntent) -> Result<SimulationOutcome> {
    let mut evm = Context::mainnet()
        .with_db(CacheDB::<EmptyDB>::default())
        .build_mainnet();

    let result = evm.transact(build_tx(intent))?.result;
    let outcome = SimulationOutcome {
        success: result.is_success(),
        gas_used: result.tx_gas_used(),
    };

    info!(
        target: "telos::settler",
        intent_id = %intent.intent_id,
        success = outcome.success,
        gas_used = outcome.gas_used,
        "simulated (empty state)",
    );

    Ok(outcome)
}

/// Replay the same tx against forked state pulled from a live RPC.
///
/// `block` selects the fork point (`BlockId::latest()` is the usual choice
/// for "what would happen right now"). The RPC URL must speak HTTP JSON-RPC.
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
        let outcome = SimulationOutcome {
            success: result.is_success(),
            gas_used: result.tx_gas_used(),
        };

        info!(
            target: "telos::settler",
            intent_id = %intent.intent_id,
            success = outcome.success,
            gas_used = outcome.gas_used,
            "simulated (forked)",
        );

        Ok(outcome)
    })
    .await
    .map_err(|e| eyre!("settler task panicked: {e}"))?
}

fn build_tx(intent: &PaymentIntent) -> TxEnv {
    TxEnv {
        caller: intent.payer,
        kind: TxKind::Call(intent.merchant),
        value: U256::ZERO,
        gas_limit: 100_000,
        gas_price: 0,
        ..Default::default()
    }
}
