//! Telos submitter — Week 7–8 broadcast + confirmation layer.
//!
//! Takes a [`SubmissionPlan`] from the settler and runs the full lifecycle:
//!   - **dry-run**: estimates gas, returns [`SettlementResult::DryRun`];
//!   - **broadcast**: signs and submits via an Alloy wallet-aware provider,
//!     awaits the receipt under a confirmations + timeout budget, decodes
//!     the `OrderPlaced` event, returns the corresponding terminal state.
//!
//! Defaults to dry-run. Broadcast requires both the construction-time
//! `broadcast=true` flag and the env wiring at the CLI — two gates by
//! design. A misconfigured submitter that silently broadcasts is the
//! worst kind of footgun.
//!
//! Nonce, gas price, and chain id are filled by Alloy's default provider
//! middleware. For high-throughput single-signer use you'd swap in a
//! cached `NonceFiller` to skip the per-tx round-trip; deferred.

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolEvent;
use eyre::{Result, WrapErr};
use std::str::FromStr;
use std::time::Duration;
use telos_settler::{PreparedTx, SubmissionPlan, abi::IHyperliquidGateway};
use telos_types::SettlementResult;
use tracing::info;

#[derive(Clone, Copy, Debug)]
pub struct ConfirmConfig {
    pub confirmations: u64,
    pub timeout: Duration,
}

impl Default for ConfirmConfig {
    fn default() -> Self {
        Self { confirmations: 1, timeout: Duration::from_secs(60) }
    }
}

#[derive(Clone)]
pub struct Submitter {
    inner: Inner,
}

#[derive(Clone)]
struct Inner {
    rpc_url: String,
    wallet: EthereumWallet,
    signer_address: Address,
    broadcast: bool,
}

impl Submitter {
    /// Build a submitter from an HTTP RPC URL and a hex private key.
    /// `broadcast=false` keeps the submitter in dry-run mode regardless of
    /// what callers later request — the safety bit lives at construction.
    pub fn new(rpc_url: String, signer_key: &str, broadcast: bool) -> Result<Self> {
        let signer = PrivateKeySigner::from_str(signer_key.trim_start_matches("0x"))
            .wrap_err("invalid TELOS_SIGNER_KEY")?;
        let signer_address = signer.address();
        let wallet = EthereumWallet::from(signer);

        Ok(Self {
            inner: Inner { rpc_url, wallet, signer_address, broadcast },
        })
    }

    pub fn signer_address(&self) -> Address {
        self.inner.signer_address
    }

    /// Estimate (dry-run) or broadcast and confirm the hedge tx.
    ///
    /// Returns the terminal [`SettlementResult`]. Errors from this method
    /// represent submitter-level problems (invalid RPC URL, signer error,
    /// RPC unreachable) — *transaction-level* failures (revert, timeout)
    /// are reported as `Result::Ok` with the corresponding variant, since
    /// they are normal operating outcomes the listener wants to log
    /// rather than retry.
    pub async fn submit_and_confirm(
        &self,
        plan: &SubmissionPlan,
        cfg: ConfirmConfig,
    ) -> Result<SettlementResult> {
        let provider = ProviderBuilder::new()
            .wallet(self.inner.wallet.clone())
            .connect_http(self.inner.rpc_url.parse().wrap_err("invalid RPC URL")?);

        let tx = build_request(self.inner.signer_address, &plan.hedge);

        if !self.inner.broadcast {
            let estimated_gas = provider
                .estimate_gas(tx)
                .await
                .wrap_err("gas estimation failed")?;
            let result = SettlementResult::DryRun {
                intent_id: plan.intent_id,
                estimated_gas,
            };
            info!(
                target: "telos::submitter",
                intent_id = %plan.intent_id,
                signer = %self.inner.signer_address,
                to = %plan.hedge.to,
                estimated_gas,
                mode = "dry-run",
                "would broadcast hedge",
            );
            return Ok(result);
        }

        let pending = provider
            .send_transaction(tx)
            .await
            .wrap_err("broadcast failed")?;
        let tx_hash = *pending.tx_hash();

        info!(
            target: "telos::submitter",
            intent_id = %plan.intent_id,
            tx_hash = %tx_hash,
            confirmations = cfg.confirmations,
            timeout_secs = cfg.timeout.as_secs(),
            "awaiting receipt",
        );

        let receipt = match pending
            .with_required_confirmations(cfg.confirmations)
            .with_timeout(Some(cfg.timeout))
            .get_receipt()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                info!(
                    target: "telos::submitter",
                    intent_id = %plan.intent_id,
                    tx_hash = %tx_hash,
                    waited_secs = cfg.timeout.as_secs(),
                    ?err,
                    "no receipt within budget",
                );
                return Ok(SettlementResult::Timeout {
                    intent_id: plan.intent_id,
                    tx_hash,
                    waited_secs: cfg.timeout.as_secs(),
                });
            }
        };

        let block_number = receipt.block_number.unwrap_or_default();
        let gas_used = receipt.gas_used;

        if !receipt.status() {
            info!(
                target: "telos::submitter",
                intent_id = %plan.intent_id,
                tx_hash = %tx_hash,
                block_number,
                gas_used,
                "tx mined but reverted",
            );
            return Ok(SettlementResult::Failed {
                intent_id: plan.intent_id,
                tx_hash,
                block_number,
            });
        }

        let hedge_acked = receipt
            .inner
            .logs()
            .iter()
            .any(|log| match IHyperliquidGateway::OrderPlaced::decode_log(&log.inner) {
                Ok(o) => o.asset == infer_asset_from_calldata(&plan.hedge),
                Err(_) => false,
            });

        info!(
            target: "telos::submitter",
            intent_id = %plan.intent_id,
            tx_hash = %tx_hash,
            block_number,
            gas_used,
            hedge_acked,
            "confirmed",
        );

        Ok(SettlementResult::Confirmed {
            intent_id: plan.intent_id,
            tx_hash,
            block_number,
            gas_used,
            hedge_acked,
        })
    }
}

fn build_request(from: Address, prepared: &PreparedTx) -> TransactionRequest {
    TransactionRequest::default()
        .with_from(from)
        .with_to(prepared.to)
        .with_input(prepared.data.clone())
        .with_gas_limit(prepared.gas_limit)
        .with_value(U256::ZERO)
}

/// The hedge calldata starts with the `placeShort` selector followed by
/// abi-encoded (asset, size, maxSlippageBps). The asset address occupies
/// bytes 4..36 (right-padded to 32). Pulling it back out lets us match
/// emitted `OrderPlaced` events without re-encoding.
fn infer_asset_from_calldata(prepared: &PreparedTx) -> Address {
    if prepared.data.len() < 36 {
        return Address::ZERO;
    }
    Address::from_slice(&prepared.data[16..36])
}

// Compile-time guard: Submitter must be Send + Sync so it can be cloned
// across listener tasks.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Submitter>();
};
