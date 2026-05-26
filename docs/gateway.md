# Gateway Guide

`NostrMCPGateway` is the simplest way to expose an MCP server through ContextVM.

It wraps `NostrServerTransport`, receives incoming ContextVM requests from Nostr, and lets your application send responses back using the inbound event id.

For native Rust applications, this is usually not the primary path. Most users should build an `rmcp` server and attach `NostrServerTransport` directly, as described in the native server guide.

## When to use it

Use the gateway when:

- you already have MCP request handling logic
- you want a straightforward server loop
- you want optional public announcements without building directly on the transport layer

Do not start here if you are writing a new native Rust MCP server from scratch.

## Minimal example

This follows the shape of the repository gateway example.

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

    let nostr_config = NostrServerTransportConfig::default()
        .with_relay_urls(vec!["wss://relay.damus.io".to_string()])
        .with_server_info(
            ServerInfo::default()
                .with_name("Echo Server".to_string())
                .with_about("A simple ContextVM server".to_string()),
        )
        .with_announced_server(true);

    let config = GatewayConfig::new(nostr_config);

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

- a message channel of `IncomingRequest`
- automatic routing of responses by original Nostr event id through `send_response()`
- optional public announcements through `announce()`

## When not to use it

Prefer the native server transport path when:

- your application is already modeled as an `rmcp` `ServerHandler`
- you want the normal `rmcp` running service lifecycle through `ServiceExt`
- you want docs and examples that match the broader `rmcp` ecosystem

## Important server config

The main operational knobs live on `NostrServerTransportConfig`:

- `relay_urls`: relays to connect to
- `encryption_mode`: plaintext vs encrypted session policy
- `gift_wrap_mode`: choose between persistent and ephemeral gift wraps
- `server_info`: metadata used in public announcements
- `is_announced_server`: auto-publish announcements on start (CEP-6)
- `allowed_public_keys`: static client allowlist
- `excluded_capabilities`: allow public access to specific methods or capability names
- `max_sessions`, `cleanup_interval`, `session_timeout`: server-side session lifecycle
- `relay_list_urls`: relay URLs advertised in kind 10002 (CEP-17); defaults to `relay_urls`
- `bootstrap_relay_urls`: additional relays for publishing announcements (CEP-6/17)
- `publish_relay_list`: whether to publish kind 10002 relay list metadata; default `true`
- `profile_metadata`: optional profile metadata for kind 0 publication (CEP-23)

## Behavioral notes

- responses are routed using the inbound request event id, not just the JSON-RPC id
- for announced servers, public metadata publication is part of the supported flow
- authorization and allowlist bypass behavior are also exercised by the integration tests

## rmcp path

If your server already uses `rmcp`, the gateway also exposes the associated function `NostrMCPGateway::serve_handler()` so you can attach a handler directly without manually running the request loop.

That said, the preferred native architecture is still `rmcp` service first and ContextVM transport second.
