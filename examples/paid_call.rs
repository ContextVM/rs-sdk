//! Test client: send a paid tools/call and print everything that comes back.
use contextvm_sdk::core::types::*;
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let server_pubkey_hex = std::env::args().nth(1).expect("server pubkey");
    let keys = signer::generate();
    let nostr_config = NostrClientTransportConfig::default()
        .with_server_pubkey(server_pubkey_hex)
        .with_encryption_mode(EncryptionMode::Optional);
    let mut proxy = NostrMCPProxy::new(keys, ProxyConfig::new(nostr_config)).await?;
    let mut rx = proxy.start().await?;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(7),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "download_media",
            "arguments": { "url": "https://www.youtube.com/watch?v=aqz-KE-bpKQ", "mode": "audio" }
        })),
    });
    println!(">>> sending tools/call download_media");
    proxy.send(&request).await?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while std::time::Instant::now() < deadline {
        let Ok(msg) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await else { continue };
        let Some(msg) = msg else { break };
        println!("<<< {}", serde_json::to_string(&msg).unwrap());
    }
    proxy.stop().await?;
    println!("done");
    Ok(())
}
