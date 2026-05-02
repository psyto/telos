//! Telos listener — Week 1 read path.
//!
//! Subscribes to new block headers from a WebSocket RPC and logs each one.
//! No execution, no decisions. The point is to internalize Alloy's typed-provider
//! pattern before adding domain logic in later weeks.

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use eyre::Result;
use futures_util::StreamExt;
use tracing::info;

/// Subscribe to new block headers from a WebSocket RPC and log each one.
///
/// Returns when the upstream stream closes (typically: connection drop).
pub async fn watch_headers(ws_url: &str) -> Result<()> {
    let ws = WsConnect::new(ws_url);
    let provider = ProviderBuilder::new().connect_ws(ws).await?;

    let sub = provider.subscribe_blocks().await?;
    let mut stream = sub.into_stream();

    info!(target: "telos::listener", url = %ws_url, "subscribed to block headers");

    while let Some(header) = stream.next().await {
        info!(
            target: "telos::listener",
            number = header.number,
            hash = %header.hash,
            gas_used = header.gas_used,
            "block",
        );
    }

    Ok(())
}
