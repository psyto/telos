//! Telos submitter — Week 7 broadcast layer.
//!
//! Takes a [`SubmissionPlan`] from the settler and either:
//!   - **dry-run**: estimates gas, logs what *would* be sent;
//!   - **broadcast**: signs and submits the hedge tx via an Alloy
//!     wallet-aware provider, returning the tx hash.
//!
//! Defaults to dry-run. Broadcast requires `TELOS_BROADCAST=1` *and* a
//! signer key — both gates exist on purpose. A misconfigured submitter
//! that silently broadcasts is the worst kind of footgun.
//!
//! Nonce, gas price, and chain id are filled by Alloy's default provider
//! middleware. For high-throughput single-signer use you'd swap in a
//! cached `NonceFiller` to skip the per-tx round-trip; deferred.

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use std::str::FromStr;
use telos_settler::{PreparedTx, SubmissionPlan};
use tracing::info;

/// What the submitter reports back. `Dry` means we estimated and logged;
/// `Sent` means the tx is in the mempool (or at least the RPC accepted it).
#[derive(Debug, Clone)]
pub enum SubmissionReceipt {
    Dry { estimated_gas: u64 },
    Sent { tx_hash: B256 },
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

    /// Estimate or send the hedge tx, depending on the broadcast flag.
    pub async fn submit(&self, plan: &SubmissionPlan) -> Result<SubmissionReceipt> {
        let provider = ProviderBuilder::new()
            .wallet(self.inner.wallet.clone())
            .connect_http(self.inner.rpc_url.parse().wrap_err("invalid RPC URL")?);

        let tx = build_request(self.inner.signer_address, &plan.hedge);

        if !self.inner.broadcast {
            let estimated_gas = provider
                .estimate_gas(tx)
                .await
                .wrap_err("gas estimation failed")?;
            info!(
                target: "telos::submitter",
                intent_id = %plan.intent_id,
                signer = %self.inner.signer_address,
                to = %plan.hedge.to,
                estimated_gas,
                mode = "dry-run",
                "would broadcast hedge",
            );
            return Ok(SubmissionReceipt::Dry { estimated_gas });
        }

        let pending = provider
            .send_transaction(tx)
            .await
            .wrap_err("broadcast failed")?;
        let tx_hash = *pending.tx_hash();

        info!(
            target: "telos::submitter",
            intent_id = %plan.intent_id,
            signer = %self.inner.signer_address,
            to = %plan.hedge.to,
            tx_hash = %tx_hash,
            mode = "broadcast",
            "hedge submitted",
        );

        Ok(SubmissionReceipt::Sent { tx_hash })
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

// Compile-time guard: Submitter must be Send + Sync so it can be cloned
// across listener tasks.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Submitter>();
};
