//! Telos node — Week 13 JSON-RPC façade.
//!
//! A minimal `eth_call`-only server that dispatches every call through a
//! REVM equipped with [`telos_precompile::TelosPrecompiles`]. Anyone can
//! point a standard JSON-RPC client (cast, ethers, alloy) at this server,
//! call into address `0x…0901`, and get the canonical intent digest back.
//!
//! What this is **not**: a real Ethereum node. There is no chain, no
//! mempool, no consensus, no persistent state. Each `eth_call` builds an
//! ephemeral EVM over `EmptyDB`, runs one transaction, and discards the
//! state. A real Reth integration would plug `TelosPrecompiles` into
//! `reth_evm::ConfigureEvm` so the same precompile is available across
//! the node's normal execution pipeline. The pattern is the same; the
//! plumbing is much heavier and is left as the next deliberate step.

pub mod rpc;
pub mod server;

pub use rpc::{EthCallRequest, TelosNode};
pub use server::{ServerConfig, run_server};
