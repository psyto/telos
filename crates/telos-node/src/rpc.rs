//! JSON-RPC method definitions and the dispatch into our custom EVM.
//!
//! `TelosRpcServer` exposes one Ethereum-style method, `eth_call`, that
//! routes every request through a fresh REVM instance whose precompile
//! provider includes `intent_digest`. The conversion from
//! [`EthCallRequest`] → `TxEnv` is intentionally minimal: we cover the
//! fields a precompile call actually needs (`to`, `data`, `from`,
//! `gas`) and ignore the rest. A full eth_call surface would also handle
//! `value`, `gasPrice`, EIP-1559 fields, access lists, and state
//! overrides — those expand the surface without adding teaching value.

use alloy::primitives::{Address, Bytes};
use jsonrpsee::core::async_trait;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use revm::context::result::{ExecutionResult, Output};
use revm::context::{Evm, TxEnv};
use revm::database::{CacheDB, EmptyDB};
use revm::handler::instructions::EthInstructions;
use revm::primitives::{TxKind, U256};
use revm::{Context, ExecuteEvm, MainContext};
use serde::{Deserialize, Serialize};
use telos_precompile::TelosPrecompiles;
use tracing::warn;

/// The subset of the standard Ethereum `eth_call` request we honour.
///
/// `to` is required — a call without a target makes no sense for our
/// stateless dispatcher. `from`, `data`, and `gas` default to caller
/// `0x0`, empty calldata, and 1M gas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EthCallRequest {
    pub to: Address,
    #[serde(default)]
    pub from: Option<Address>,
    #[serde(default)]
    pub data: Option<Bytes>,
    #[serde(default, alias = "gasLimit")]
    pub gas: Option<u64>,
}

/// JSON-RPC surface. Following Ethereum conventions, the method names
/// are namespaced by the standard `eth_` prefix; the macro generates
/// the trait + dispatch glue.
#[rpc(server)]
pub trait TelosRpc {
    #[method(name = "eth_chainId")]
    async fn chain_id(&self) -> Result<String, ErrorObjectOwned>;

    #[method(name = "eth_call")]
    async fn eth_call(&self, req: EthCallRequest) -> Result<String, ErrorObjectOwned>;
}

/// Concrete implementation of [`TelosRpc`]. Named `TelosNode` because
/// `#[rpc(server)]` already generates a trait called `TelosRpcServer`
/// from the `TelosRpc` trait above — the suffix is reserved by the
/// macro. Cloneable because each RPC invocation gets its own `&self`
/// and we want cheap fan-out across tokio tasks.
#[derive(Clone)]
pub struct TelosNode {
    chain_id: u64,
}

impl TelosNode {
    pub fn new(chain_id: u64) -> Self {
        Self { chain_id }
    }
}

#[async_trait]
impl TelosRpcServer for TelosNode {
    async fn chain_id(&self) -> Result<String, ErrorObjectOwned> {
        Ok(format!("0x{:x}", self.chain_id))
    }

    async fn eth_call(&self, req: EthCallRequest) -> Result<String, ErrorObjectOwned> {
        let output = run_eth_call(req).map_err(|e| {
            warn!(target: "telos::node", ?e, "eth_call dispatch failed");
            ErrorObjectOwned::owned(-32000, format!("call failed: {e}"), None::<()>)
        })?;
        Ok(format!("0x{}", hex_encode(&output)))
    }
}

/// Dispatch one call through a fresh REVM with TelosPrecompiles.
///
/// Stateless by design: we do not want one RPC client to poison another
/// client's state. Each call builds its own `Context::mainnet().with_db(EmptyDB)`,
/// runs the tx, and drops the state on the floor. The call output is
/// what `eth_call` returns; the state changes are discarded the same
/// way they would be on a real node when the call is `staticcall`-shaped.
fn run_eth_call(req: EthCallRequest) -> Result<Bytes, String> {
    let ctx = Context::mainnet().with_db(CacheDB::<EmptyDB>::default());
    let spec = *ctx.cfg.spec();
    let mut evm = Evm::new(
        ctx,
        EthInstructions::new_mainnet_with_spec(spec),
        TelosPrecompiles::new(spec),
    );

    let tx = TxEnv {
        caller: req.from.unwrap_or(Address::ZERO),
        kind: TxKind::Call(req.to),
        value: U256::ZERO,
        data: req.data.unwrap_or_default(),
        gas_limit: req.gas.unwrap_or(1_000_000),
        gas_price: 0,
        ..Default::default()
    };

    let result = evm
        .transact(tx)
        .map_err(|e| format!("transact: {e}"))?
        .result;

    match result {
        ExecutionResult::Success { output: Output::Call(b), .. } => Ok(b),
        ExecutionResult::Success { output: Output::Create(b, _), .. } => Ok(b),
        ExecutionResult::Revert { output, .. } => {
            Err(format!("revert: 0x{}", hex_encode(&output)))
        }
        ExecutionResult::Halt { reason, .. } => Err(format!("halt: {reason:?}")),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}
