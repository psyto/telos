use eyre::Result;
use telos_node::{ServerConfig, run_server};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cfg = ServerConfig {
        bind: parse_bind(),
        chain_id: parse_chain_id(),
    };

    let (_addr, handle) = run_server(cfg).await?;
    handle.stopped().await;
    Ok(())
}

fn parse_bind() -> std::net::SocketAddr {
    std::env::var("TELOS_NODE_BIND")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| ([127, 0, 0, 1], 8545).into())
}

fn parse_chain_id() -> u64 {
    std::env::var("TELOS_NODE_CHAIN_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x7e105)
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
