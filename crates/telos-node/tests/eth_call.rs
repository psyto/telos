//! Spin up the real server, make an eth_call against the precompile,
//! assert the response equals an off-chain keccak. End-to-end proof
//! that a JSON-RPC client can drive the custom EVM.

use alloy::primitives::keccak256;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;
use serde_json::json;
use telos_node::{ServerConfig, run_server};
use telos_precompile::{INTENT_DIGEST_INPUT_LEN, TELOS_INTENT_DIGEST_ADDRESS};

#[tokio::test]
async fn eth_call_to_intent_digest_returns_canonical_keccak() {
    let cfg = ServerConfig {
        bind: ([127, 0, 0, 1], 0).into(), // ":0" — let the OS pick a port
        chain_id: 31337,
    };
    let (addr, handle) = run_server(cfg).await.expect("server starts");

    let url = format!("http://{addr}");
    let client = HttpClientBuilder::default().build(&url).expect("client builds");

    let calldata = vec![0x42u8; INTENT_DIGEST_INPUT_LEN];
    let calldata_hex = format!("0x{}", hex::encode(&calldata));

    let req = json!({
        "to": TELOS_INTENT_DIGEST_ADDRESS,
        "data": calldata_hex,
        "gas": 100_000,
    });

    let resp: String = client
        .request("eth_call", rpc_params![req])
        .await
        .expect("eth_call succeeds");

    let expected = keccak256(calldata.as_slice());
    let expected_hex = format!("0x{}", hex::encode(expected.as_slice()));
    assert_eq!(resp, expected_hex, "digest must match off-chain keccak");

    handle.stop().expect("server stops");
}

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            s.push_str(&format!("{byte:02x}"));
        }
        s
    }
}
