//! Example: Expose an echo MCP tool via Nostr gateway.
//!
//! This demonstrates how to create a ContextVM gateway that receives
//! MCP requests over Nostr and responds to them.
//!
//! Usage: cargo run --example gateway -- [--log-file <path>]

use contextvm_sdk::core::types::*;
use contextvm_sdk::gateway::{GatewayConfig, NostrMCPGateway};
use contextvm_sdk::signer;
use contextvm_sdk::transport::server::NostrServerTransportConfig;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut log_file_path: Option<String> = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--log-file" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    panic!("Usage: gateway [--log-file <path>]");
                };
                log_file_path = Some(path.clone());
            }
            other => {
                panic!("Unknown argument: {other}. Usage: gateway [--log-file <path>]");
            }
        }
        index += 1;
    }

    // Generate ephemeral keys for this session
    let keys = signer::generate();
    println!("Server pubkey: {}", keys.public_key().to_hex());

    // Configure the gateway
    let mut nostr_config = NostrServerTransportConfig::default()
        .with_server_info(
            ServerInfo::default()
                .with_name("Echo Server")
                .with_about("A simple echo tool exposed via ContextVM"),
        )
        .with_announced_server(true);
    if let Some(path) = log_file_path {
        nostr_config = nostr_config.with_log_file_path(path);
    }
    let config = GatewayConfig::new(nostr_config);

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
