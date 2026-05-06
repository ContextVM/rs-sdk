# Gateway Guide

[`NostrMCPGateway`](src/gateway/mod.rs:20) is the simplest way to expose an MCP server through ContextVM.

It wraps [`NostrServerTransport`](src/transport/server/mod.rs:87), receives incoming ContextVM requests from Nostr, and lets your application send responses back using the inbound event id.

For native Rust applications, this is usually not the primary path. Most users should build an `rmcp` server and attach [`NostrServerTransport`](src/transport/server/mod.rs:87) directly, as described in [`server-transport.md`](docs/server-transport.md).

## When to use it

Use the gateway when:

- you already have MCP request handling logic
- you want a straightforward server loop
- you want optional public announcements without building directly on the transport layer

Do not start here if you are writing a new native Rust MCP server from scratch.

## Minimal example

This follows the shape of [`examples/gateway.rs`](examples/gateway.rs).

```rust
use contextvm_sdk::core::types::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcResponse, ServerInfo,
};
use contextvm_sdk::gateway::{GatewayConfig, NostrMCPGateway};
use contextvm_sdk::signer;
use contextvm_sdk::transport::server::NostrServerTransportConfig;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::generate();

    let config = GatewayConfig {
        nostr_config: NostrServerTransportConfig {
            relay_urls: vec!["wss://relay.damus.io".to_string()],
            server_info: Some(ServerInfo {
                name: Some("Echo Server".to_string()),
                about: Some("A simple ContextVM server".to_string()),
                ..Default::default()
            }),
            is_announced_server: true,
            ..Default::default()
        },
    };

    let mut gateway = NostrMCPGateway::new(keys, config).await?;
    let mut rx = gateway.start().await?;
    gateway.announce().await?;

    while let Some(req) = rx.recv().await {
        let response = match &req.message {
            JsonRpcMessage::Request(request) if request.method == "tools/list" => {
                JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id.clone(),
                    result: serde_json::json!({
                        "tools": [{
                            "name": "echo",
                            "description": "Echo a message",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "message": { "type": "string" }
                                },
                                "required": ["message"]
                            }
                        }]
                    }),
                })
            }
            JsonRpcMessage::Request(request) => JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                error: JsonRpcError {
                    code: -32601,
                    message: "Method not found".to_string(),
                    data: None,
                },
            }),
            _ => continue,
        };

        gateway.send_response(&req.event_id, response).await?;
    }

    Ok(())
}
```

## What the gateway gives you

- a message channel of [`IncomingRequest`](src/transport/server/mod.rs:113)
- automatic routing of responses by original Nostr event id through [`send_response()`](src/gateway/mod.rs:57)
- optional public announcements through [`announce()`](src/gateway/mod.rs:62)

## When not to use it

Prefer [`server-transport.md`](docs/server-transport.md) when:

- your application is already modeled as an `rmcp` [`ServerHandler`](rust-sdk/crates/rmcp/src/lib.rs:16)
- you want the normal `rmcp` running service lifecycle through [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20)
- you want docs and examples that match the broader `rmcp` ecosystem

## Important server config

The main operational knobs live on [`NostrServerTransportConfig`](src/transport/server/mod.rs:36):

- `relay_urls`: relays to connect to
- `encryption_mode`: plaintext vs encrypted session policy
- `gift_wrap_mode`: choose between persistent and ephemeral gift wraps
- `server_info`: metadata used in public announcements
- `is_announced_server`: publish public discovery events
- `allowed_public_keys`: static client allowlist
- `excluded_capabilities`: allow public access to specific methods or capability names
- `max_sessions`, `cleanup_interval`, `session_timeout`: server-side session lifecycle

## Behavioral notes

- responses are routed using the inbound request event id, not just the JSON-RPC id
- for announced servers, public metadata publication is part of the supported flow, verified in [`tests/transport_integration.rs`](tests/transport_integration.rs)
- authorization and allowlist bypass behavior are also exercised in [`tests/transport_integration.rs`](tests/transport_integration.rs)

## rmcp path

If your server already uses `rmcp`, the gateway also exposes [`serve_handler()`](src/gateway/mod.rs:88) so you can attach a handler directly without manually running the request loop.

That said, the preferred native documentation path is still [`server-transport.md`](docs/server-transport.md), because it reflects the main architecture: `rmcp` service first, ContextVM transport second.
