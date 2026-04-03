//! Example: Expose an echo MCP tool via Nostr gateway.
//!
//! This demonstrates how to create a ContextVM gateway that receives
//! MCP requests over Nostr and responds to them.

use contextvm_sdk::core::types::*;
use contextvm_sdk::gateway::{GatewayConfig, NostrMCPGateway};
use contextvm_sdk::signer;
use contextvm_sdk::transport::server::NostrServerTransportConfig;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    tracing_subscriber::fmt::init();

    // Generate ephemeral keys for this session
    let keys = signer::generate();
    println!("Server pubkey: {}", keys.public_key().to_hex());

    // Configure the gateway
    let config = GatewayConfig {
        nostr_config: NostrServerTransportConfig {
            relay_urls: vec!["wss://relay.damus.io".to_string()],
            server_info: Some(ServerInfo {
                name: Some("Echo Server".to_string()),
                about: Some("A simple echo tool exposed via ContextVM".to_string()),
                ..Default::default()
            }),
            is_announced_server: true,
            ..Default::default()
        },
    };

    let mut gateway = NostrMCPGateway::new(keys, config).await?;
    let mut rx = gateway.start().await?;

    // Publish server announcement
    let announcement_id = gateway.announce().await?;
    println!("Published announcement: {announcement_id}");

    println!("Gateway running. Waiting for requests...");

    // Process incoming requests
    while let Some(req) = rx.recv().await {
        println!(
            "Request from {}: {:?}",
            &req.client_pubkey[..8],
            req.message.method()
        );

        let response = match &req.message {
            JsonRpcMessage::Request(r) => match r.method.as_str() {
                "initialize" => JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: r.id.clone(),
                    result: serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "Echo Server", "version": "0.1.0" }
                    }),
                }),
                "tools/list" => JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: r.id.clone(),
                    result: serde_json::json!({
                        "tools": [{
                            "name": "echo",
                            "description": "Echoes back the input message",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "message": { "type": "string", "description": "Message to echo" }
                                },
                                "required": ["message"]
                            }
                        }]
                    }),
                }),
                "tools/call" => {
                    let message = r
                        .params
                        .as_ref()
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("(no message)");

                    JsonRpcMessage::Response(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: r.id.clone(),
                        result: serde_json::json!({
                            "content": [{ "type": "text", "text": format!("Echo: {message}") }]
                        }),
                    })
                }
                _ => JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
                    jsonrpc: "2.0".to_string(),
                    id: r.id.clone(),
                    error: JsonRpcError {
                        code: -32601,
                        message: "Method not found".to_string(),
                        data: None,
                    },
                }),
            },
            // Ignore notifications
            _ => continue,
        };

        gateway.send_response(&req.event_id, response).await?;
    }

    Ok(())
}
