//! Telos settler — Week 3 simulation harness.
//!
//! Given a [`telos_types::PaymentIntent`], this crate stands up an in-memory
//! REVM and replays the merchant settlement transfer against empty state to
//! prove the harness is wired end-to-end. The next step (forking real Tempo
//! state, decoding actual swap calldata, sizing the perp hedge) lands in
//! follow-up commits — the API surface is fixed here so callers can be
//! built against it now.

use eyre::Result;
use revm::{
    Context, ExecuteEvm, MainBuilder, MainContext,
    context::TxEnv,
    database::{CacheDB, EmptyDB},
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

    let tx = TxEnv {
        caller: intent.payer,
        kind: TxKind::Call(intent.merchant),
        value: U256::ZERO,
        gas_limit: 100_000,
        gas_price: 0,
        ..Default::default()
    };

    let result = evm.transact(tx)?.result;
    let outcome = SimulationOutcome {
        success: result.is_success(),
        gas_used: result.tx_gas_used(),
    };

    info!(
        target: "telos::settler",
        intent_id = %intent.intent_id,
        success = outcome.success,
        gas_used = outcome.gas_used,
        "simulated",
    );

    Ok(outcome)
}
