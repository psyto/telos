//! Shared wire-format types that flow between listener, settler, and precompile.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

/// A merchant's declared end: settle this payment, in this asset, for this amount,
/// within this slippage budget, before this deadline.
///
/// Intents are venue-agnostic — the solver chooses spot route and hedge sizing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentIntent {
    pub payer: Address,
    pub merchant: Address,
    pub settlement_asset: Address,
    pub settlement_amount: U256,
    pub max_slippage_bps: u16,
    pub deadline: u64,
}
