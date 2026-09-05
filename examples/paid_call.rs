//! Test client: send a paid tools/call, print every notification/response until
//! the final JSON-RPC response (id echo) or timeout. Usage: paid_call <pubkey> [secs]
use contextvm_sdk::core::types::*;
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let server_pubkey_hex = std::env::args().nth(1).expect("server pubkey");
    let wait_secs: u64 = std::env::args()
        .nth(2)
        .map(|s| s.parse().unwrap())
        .unwrap_or(300);
    let keys = signer::generate();
    let nostr_config = NostrClientTransportConfig::default()
        .with_server_pubkey(server_pubkey_hex)
        .with_encryption_mode(EncryptionMode::Optional);
    let mut proxy = NostrMCPProxy::new(keys, ProxyConfig::new(nostr_config)).await?;
    let mut rx = proxy.start().await?;

    let id = serde_json::json!(7);
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: id.clone(),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "download_media",
            "arguments": { "url": "https://www.youtube.com/watch?v=aqz-KE-bpKQ", "mode": "audio" }
        })),
    });
    println!(">>> sending tools/call download_media (audio)");
    proxy.send(&request).await?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    while std::time::Instant::now() < deadline {
        let Ok(Some(msg)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
        else {
            continue;
        };
        let s = serde_json::to_string(&msg).unwrap();
        println!("<<< {s}");
        match &msg {
            JsonRpcMessage::Response(r) if r.id == id => {
                println!("FINAL RESPONSE RECEIVED");
                break;
            }
            JsonRpcMessage::ErrorResponse(e) if e.id == id => {
                println!("ERROR RESPONSE RECEIVED");
                break;
            }
            _ => {}
        }
    }
    proxy.stop().await?;
    println!("done");
    Ok(())
}
