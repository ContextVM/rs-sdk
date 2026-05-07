# Native Client Guide

Use this path when you are building a native ContextVM client in Rust.

The recommended architecture is:

- define an `rmcp` client handler or use a lightweight client info object
- create a `NostrClientTransport`
- attach the transport with `rmcp`'s `ServiceExt`

This follows the same pattern as the standard `rmcp` client examples, except the transport is ContextVM over Nostr instead of HTTP.

## The high-level shape

In `rmcp`, a client is typically started with `client_info.serve(transport)`.

For ContextVM, the transport becomes `NostrClientTransport`. In the current SDK API, you pass that transport directly to `ServiceExt`; there is no extra adapter step in the public workflow.

## Loading an existing private key

The signer helper is not limited to ephemeral keys. If you already have a private key, load it with `from_sk()`.

It accepts either:

- a 64-character hex secret key
- an `nsec` bech32 secret key

```rust
use contextvm_sdk::signer;

let signer = signer::from_sk("<hex-or-nsec-private-key>")?;
println!("client pubkey: {}", signer.public_key().to_hex());
```

Use `generate()` only when you explicitly want a new random identity for a short-lived client or test flow.

## Example

```rust
use contextvm_sdk::transport::client::{
    NostrClientTransport, NostrClientTransportConfig,
};
use contextvm_sdk::{EncryptionMode, GiftWrapMode};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation},
};

#[derive(Clone, Default)]
struct DemoClient;

impl ClientHandler for DemoClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("demo-client", "0.1.0"),
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let signer = contextvm_sdk::signer::generate();

    let transport = NostrClientTransport::new(
        signer,
        NostrClientTransportConfig {
            relay_urls: vec!["wss://relay.primal.net".to_string()],
            server_pubkey: "<server-hex-pubkey>".to_string(),
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            ..Default::default()
        },
    )
    .await?;

    let client = DemoClient.serve(transport).await?;

    let server_info = client.peer_info();
    println!("Connected to server: {server_info:#?}");

    let tools = client.list_tools(Default::default()).await?;
    println!("Available tools: {tools:#?}");

    let result = client
        .call_tool(
            CallToolRequestParams::new("echo")
                .with_arguments(serde_json::json!({ "message": "hello" }).as_object().cloned().unwrap()),
        )
        .await?;

    println!("Tool result: {result:#?}");
    client.cancel().await?;
    Ok(())
}
```

This is the ContextVM equivalent of the usual `rmcp` client workflow, but using `NostrClientTransport` directly.

## What the transport adds

`NostrClientTransport` adds ContextVM-specific client behavior on top of `rmcp` client semantics:

- relay connection management via `NostrClientTransport::new()`
- target server selection through `server_pubkey` in `NostrClientTransportConfig`
- request and response correlation via `send()`
- server capability learning from discovery tags
- optional stateless behavior via `is_stateless` in `NostrClientTransportConfig`
- encrypted message reception and gift-wrap deduplication during notification handling

## Configuration fields that matter first

Start with these fields in `NostrClientTransportConfig`:

- `relay_urls`: relays the client uses to reach the server
- `server_pubkey`: the target server public key
- `encryption_mode`: whether plaintext is allowed
- `gift_wrap_mode`: whether to use persistent or ephemeral wrapping
- `is_stateless`: whether initialize is emulated locally for stateless workflows
- `timeout`: how long request correlation waits for a response

## When to use this instead of the proxy

Use this page's approach when you are writing a new Rust MCP client that should speak ContextVM natively.

Use the proxy guide when you want a simpler message-oriented bridge and do not want the full `rmcp` running client model.

## Behavioral notes

- The client-side `rmcp` handshake is driven by the normal `serve_client()` flow.
- The initialize request is sent automatically as part of the running client startup sequence.
- Stateless initialization behavior is covered by the conformance tests.
- Capability learning and gift-wrap handling happen inside the client transport implementation.
