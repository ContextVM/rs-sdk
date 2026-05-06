# Transport Guide

Use this page when you want to understand the transport layer itself.

If you are building a normal native server or client, start with [`server-transport.md`](docs/server-transport.md) or [`client-transport.md`](docs/client-transport.md) first.

- [`NostrClientTransport`](src/transport/client/mod.rs:69): client-side direct transport
- [`NostrServerTransport`](src/transport/server/mod.rs:87): server-side direct transport

## Why use the transport layer directly

Use transports directly when you need to:

- integrate with your own request loop
- control announcement timing yourself
- tune authorization and session behavior
- embed the transport in higher-level abstractions

## Low-level client transport example

```rust
use contextvm_sdk::core::types::{JsonRpcMessage, JsonRpcRequest};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::{
    NostrClientTransport, NostrClientTransportConfig,
};

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::generate();
    let mut transport = NostrClientTransport::new(
        keys,
        NostrClientTransportConfig {
            relay_urls: vec!["wss://relay.damus.io".to_string()],
            server_pubkey: "<server-hex-pubkey>".to_string(),
            ..Default::default()
        },
    )
    .await?;

    transport.start().await?;
    let mut rx = transport.take_message_receiver().expect("receiver available");

    transport
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "tools/list".to_string(),
            params: None,
        }))
        .await?;

    if let Some(message) = rx.recv().await {
        println!("received: {:?}", message);
    }

    transport.close().await?;
    Ok(())
}
```

## Low-level server transport example

```rust
use contextvm_sdk::core::types::{JsonRpcMessage, JsonRpcResponse};
use contextvm_sdk::signer;
use contextvm_sdk::transport::server::{
    NostrServerTransport, NostrServerTransportConfig,
};

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::generate();
    let mut transport = NostrServerTransport::new(
        keys,
        NostrServerTransportConfig {
            is_announced_server: true,
            ..Default::default()
        },
    )
    .await?;

    transport.start().await?;
    let mut rx = transport.take_message_receiver().expect("receiver available");

    while let Some(req) = rx.recv().await {
        if let Some(id) = req.message.id() {
            transport
                .send_response(
                    &req.event_id,
                    JsonRpcMessage::Response(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: serde_json::json!({"ok": true}),
                    }),
                )
                .await?;
        }
    }

    Ok(())
}
```

## Server-side semantics to understand

The server transport does more than relay bytes.

It manages:

- multi-client session state
- request route storage
- authorization via `allowed_public_keys`
- allowlist bypasses via [`CapabilityExclusion`](src/core/types.rs:269)
- announcement publication
- encryption negotiation and response mirroring

Those behaviors are visible in the fields of [`NostrServerTransport`](src/transport/server/mod.rs:87) and are exercised heavily in [`tests/transport_integration.rs`](tests/transport_integration.rs).

## What the server receives

Incoming traffic is delivered as [`IncomingRequest`](src/transport/server/mod.rs:113), which includes:

- the parsed [`JsonRpcMessage`](src/core/types.rs:146)
- the client pubkey
- the original Nostr request event id
- whether the incoming message was encrypted

That extra metadata is what allows correct response routing and encryption mirroring.
