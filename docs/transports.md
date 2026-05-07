# Transport Guide

Use this page when you want to understand the transport layer itself.

If you are building a normal native server or client, start with the dedicated native server or native client guide first.

- `NostrClientTransport`: client-side direct transport
- `NostrServerTransport`: server-side direct transport

## Why use the transport layer directly

Use transports directly when you need to:

- integrate with your own request loop
- control announcement timing yourself
- tune authorization and session behavior
- embed the transport in higher-level abstractions

## Signer choice

All transport constructors accept an existing signer, not just a newly generated one.

- Use `generate()` for temporary identities in examples, tests, and short-lived sessions.
- Use `from_sk()` when you need a stable identity backed by a pre-existing hex or `nsec` private key.

```rust
use contextvm_sdk::signer;

let signer = signer::from_sk("<hex-or-nsec-private-key>")?;
```

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
- allowlist bypasses via `CapabilityExclusion`
- announcement publication
- encryption negotiation and response mirroring

Those behaviors are part of the server transport implementation and are exercised heavily by the integration tests.

## What the server receives

Incoming traffic is delivered as `IncomingRequest`, which includes:

- the parsed `JsonRpcMessage`
- the client pubkey
- the original Nostr request event id
- whether the incoming message was encrypted

That extra metadata is what allows correct response routing and encryption mirroring.
