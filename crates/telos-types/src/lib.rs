//! Shared wire-format types that flow between listener, settler, and precompile.

use alloy::primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

/// A merchant's declared end: settle this payment, in this asset, for this amount,
/// within this slippage budget, before this deadline.
///
/// Intents are venue-agnostic — the solver chooses spot route and hedge sizing.
/// `intent_id` is the on-chain identifier emitted by the originating contract
/// and used by every downstream component to correlate state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentIntent {
    pub intent_id: B256,
    pub payer: Address,
    pub merchant: Address,
    pub settlement_asset: Address,
    pub settlement_amount: U256,
    pub max_slippage_bps: u16,
    pub deadline: u64,
}

/// A spot fill on Hyperliquid, surfaced through a HyperEVM bridge or builder
/// contract. Prices and sizes are kept as raw fixed-point — HL's native scaling
/// is 1e8 — so callers can decide whether to convert lossy or stay lossless.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub trader: Address,
    pub order_id: B256,
    pub asset: Address,
    pub is_buy: bool,
    pub price_e8: u64,
    pub size_e8: u64,
    pub timestamp: u64,
}

/// Snapshot of an asset's price, used by the settler to size hedges.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PriceQuote {
    pub asset: Address,
    pub price_e8: u64,
    pub timestamp: u64,
    pub source: PriceSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PriceSource {
    /// Last trade price observed on a Hyperliquid Fill event.
    HlFill,
}

/// The settler's plan for an intent: spot leg amount + perp short to neutralize
/// price risk during the settlement window.
///
/// The 1:1 sizing here is deliberate placeholder — production would tilt by
/// funding rate, slippage budget, and merchant payout window. The shape is
/// fixed so downstream code can be written against it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteQuote {
    pub intent_id: B256,
    pub spot_asset: Address,
    pub spot_amount: U256,
    pub hedge_size: U256,
    pub hedge_venue: Address,
    pub price_e8: u64,
    pub price_age_secs: u64,
}

/// What eventually happened to a hedge tx.
///
/// `DryRun` means the submitter was in safety mode — gas was estimated but
/// nothing was sent. `Confirmed` means the receipt landed and the
/// `OrderPlaced` event we wanted was emitted. `Failed` means the tx mined
/// but reverted (the receipt's `status == 0`); `Timeout` means we never
/// observed a receipt within the budget. The four cases cover every
/// outcome a single submission can have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SettlementResult {
    DryRun {
        intent_id: B256,
        estimated_gas: u64,
    },
    Confirmed {
        intent_id: B256,
        tx_hash: B256,
        block_number: u64,
        gas_used: u64,
        hedge_acked: bool,
    },
    Failed {
        intent_id: B256,
        tx_hash: B256,
        block_number: u64,
    },
    Timeout {
        intent_id: B256,
        tx_hash: B256,
        waited_secs: u64,
    },
}
