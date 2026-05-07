//! Example: Connect to a remote MCP server via Nostr and call tools/list.
//!
//! Usage: cargo run --example proxy -- <server_pubkey_hex>

use contextvm_sdk::core::types::*;
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    tracing_subscriber::fmt::init();

    let server_pubkey_hex = std::env::args()
        .nth(1)
        .expect("Usage: proxy <server_pubkey_hex>");

    let keys = signer::generate();
    println!("Client pubkey: {}", keys.public_key().to_hex());

    let nostr_config = NostrClientTransportConfig::default()
        .with_server_pubkey(server_pubkey_hex)
        .with_encryption_mode(EncryptionMode::Optional);
    let config = ProxyConfig::new(nostr_config);

    let mut proxy = NostrMCPProxy::new(keys, config).await?;
    let mut rx = proxy.start().await?;

    // Send tools/list request
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: "tools/list".to_string(),
        params: None,
    });

    println!("Sending tools/list request...");
    proxy.send(&request).await?;

    // Wait for response
    if let Some(response) = rx.recv().await {
        println!(
            "Response: {}",
            serde_json::to_string_pretty(&response).unwrap()
        );
    }

    proxy.stop().await?;
    Ok(())
}
