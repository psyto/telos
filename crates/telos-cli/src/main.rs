use eyre::Result;
use telos_listener::watch_headers;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let ws_url = std::env::var("TELOS_WS_URL")
        .unwrap_or_else(|_| "wss://ethereum-rpc.publicnode.com".to_string());

    watch_headers(&ws_url).await
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
