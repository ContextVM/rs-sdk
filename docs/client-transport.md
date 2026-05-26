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
use anyhow::Context;
use contextvm_sdk::transport::client::{
    NostrClientTransport, NostrClientTransportConfig,
};
use contextvm_sdk::{signer, EncryptionMode, GiftWrapMode};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult},
    ClientHandler, ServiceExt,
};

const RELAY_URL: &str = "wss://relay.contextvm.org";

#[derive(Clone, Default)]
struct DemoClient;

impl ClientHandler for DemoClient {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server_pubkey = std::env::args()
        .nth(1)
        .context("Usage: native_echo_client <server_pubkey_hex>")?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("contextvm_sdk=info".parse()?)
                .add_directive("rmcp=warn".parse()?),
        )
        .init();

    let signer = signer::generate();

    println!("Native ContextVM echo client starting");
    println!("Relay: {RELAY_URL}");
    println!("Client pubkey: {}", signer.public_key().to_hex());
    println!("Target server pubkey: {server_pubkey}");

    let transport = NostrClientTransport::new(
        signer,
        NostrClientTransportConfig::default()
            .with_relay_urls(vec![RELAY_URL.to_string()])
            .with_server_pubkey(server_pubkey)
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional),
    )
    .await?;

    let client = DemoClient.serve(transport).await?;

    let peer_info = client
        .peer_info()
        .expect("server did not provide peer info after initialize");
    println!("Connected to: {:?}", peer_info.server_info.name);

    let tools = client.list_all_tools().await?;
    println!("Discovered {} tool(s):", tools.len());
    for tool in &tools {
        println!("- {}", tool.name);
    }

    let result = client
        .call_tool(CallToolRequestParams {
            name: "echo".into(),
            arguments: serde_json::from_value(serde_json::json!({
                "message": "hello from native contextvm client"
            }))
            .ok(),
            meta: None,
            task: None,
        })
        .await?;

    println!("Echo result: {}", first_text(&result));
    client.cancel().await?;
    Ok(())
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|content| {
            if let rmcp::model::RawContent::Text(text) = &content.raw {
                Some(text.text.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
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

- `relay_urls`: relays the client uses to reach the server (empty = use relay resolution)
- `server_pubkey`: the target server's public key (hex, npub, or nprofile with relay hints)
- `encryption_mode`: whether plaintext is allowed
- `gift_wrap_mode`: whether to use persistent or ephemeral wrapping
- `is_stateless`: whether initialize is emulated locally for stateless workflows
- `timeout`: how long request correlation waits for a response
- `discovery_relay_urls`: bootstrap relays for CEP-17 kind 10002 relay-list discovery (defaults to `DEFAULT_BOOTSTRAP_RELAY_URLS`)
- `fallback_operational_relay_urls`: non-authoritative relays probed in parallel with CEP-17 discovery

## When to use this instead of the proxy

Use this page's approach when you are writing a new Rust MCP client that should speak ContextVM natively.

Use the proxy guide when you want a simpler message-oriented bridge and do not want the full `rmcp` running client model.

## Behavioral notes

- The client-side `rmcp` handshake is driven by `ServiceExt::serve()` on the client handler.
- The initialize request is sent automatically as part of the running client startup sequence.
- Stateless initialization behavior is covered by the conformance tests.
- Capability learning and gift-wrap handling happen inside the client transport implementation.
- When `relay_urls` is empty, `start()` runs 6-stage relay resolution before connecting: configured relays > nprofile hints > CEP-17 kind 10002 discovery > fallback probing > bootstrap defaults. Callers can set `server_pubkey` to an nprofile and omit `relay_urls` entirely.
