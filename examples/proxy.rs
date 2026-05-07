//! Example: Connect to a remote MCP server via Nostr and call tools/list.
//!
//! Usage: cargo run --example proxy -- <server_pubkey_hex> [--log-file <path>]

use contextvm_sdk::core::types::*;
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;
#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut server_pubkey_hex: Option<String> = None;
    let mut log_file_path: Option<String> = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--log-file" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    panic!("Usage: proxy <server_pubkey_hex> [--log-file <path>]");
                };
                log_file_path = Some(path.clone());
            }
            value => {
                if server_pubkey_hex.is_none() {
                    server_pubkey_hex = Some(value.to_string());
                } else {
                    panic!(
                        "Unknown argument: {value}. Usage: proxy <server_pubkey_hex> [--log-file <path>]"
                    );
                }
            }
        }
        index += 1;
    }

    let server_pubkey_hex =
        server_pubkey_hex.expect("Usage: proxy <server_pubkey_hex> [--log-file <path>]");

    let keys = signer::generate();
    println!("Client pubkey: {}", keys.public_key().to_hex());

    let mut nostr_config = NostrClientTransportConfig::default()
        .with_server_pubkey(server_pubkey_hex)
        .with_encryption_mode(EncryptionMode::Optional);
    if let Some(path) = log_file_path {
        nostr_config = nostr_config.with_log_file_path(path);
    }
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
