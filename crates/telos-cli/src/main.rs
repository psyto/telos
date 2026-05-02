use alloy::primitives::Address;
use eyre::{Result, WrapErr};
use telos_listener::{watch_headers, watch_intents};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let ws_url = std::env::var("TELOS_WS_URL")
        .unwrap_or_else(|_| "wss://ethereum-rpc.publicnode.com".to_string());

    match std::env::var("TELOS_INTENT_CONTRACT").ok() {
        Some(addr) => {
            let contract: Address = addr
                .parse()
                .wrap_err("TELOS_INTENT_CONTRACT must be a hex address")?;
            watch_intents(&ws_url, contract).await
        }
        None => watch_headers(&ws_url).await,
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
