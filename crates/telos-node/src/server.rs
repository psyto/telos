//! HTTP server bootstrap. `run_server` binds to an address, registers the
//! [`TelosRpcServer`] methods, and returns a handle the caller can
//! `stopped().await` on. We expose the bound address back to the caller
//! so tests can use `:0` (let the OS pick a free port) and still know
//! where to dial.

use crate::rpc::{TelosNode, TelosRpcServer};
use eyre::{Result, WrapErr};
use jsonrpsee::server::{ServerBuilder, ServerHandle};
use std::net::SocketAddr;
use tracing::info;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub chain_id: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: ([127, 0, 0, 1], 8545).into(),
            chain_id: 0x7e105, // 'telos' as hex-ish — arbitrary placeholder
        }
    }
}

pub async fn run_server(cfg: ServerConfig) -> Result<(SocketAddr, ServerHandle)> {
    let server = ServerBuilder::default()
        .build(cfg.bind)
        .await
        .wrap_err("server bind failed")?;
    let addr = server.local_addr().wrap_err("local_addr unavailable")?;

    let handle = server.start(TelosNode::new(cfg.chain_id).into_rpc());

    info!(
        target: "telos::node",
        %addr,
        chain_id = cfg.chain_id,
        "telos-node listening",
    );

    Ok((addr, handle))
}
