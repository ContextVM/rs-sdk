# Proxy Guide

[`NostrMCPProxy`](src/proxy/mod.rs:17) is the simplest way to talk to a remote ContextVM server from Rust.

It wraps [`NostrClientTransport`](src/transport/client/mod.rs:69), gives you a receiver for responses and notifications, and handles transport startup and shutdown.

For native Rust applications, this is usually not the primary path. Most users should build an `rmcp` client and attach [`NostrClientTransport`](src/transport/client/mod.rs:69) directly, as described in [`client-transport.md`](docs/client-transport.md).

## When to use it

Use the proxy when:

- you already know the target server pubkey
- you want a lightweight request/response flow
- you do not need low-level transport hooks

Do not start here if you are writing a new native Rust MCP client from scratch.

## Minimal example

This follows [`examples/proxy.rs`](examples/proxy.rs).

```rust
use contextvm_sdk::core::types::{JsonRpcMessage, JsonRpcRequest};
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::generate();

    let config = ProxyConfig {
        nostr_config: NostrClientTransportConfig {
            relay_urls: vec!["wss://relay.damus.io".to_string()],
            server_pubkey: "<server-hex-pubkey>".to_string(),
            ..Default::default()
        },
    };

    let mut proxy = NostrMCPProxy::new(keys, config).await?;
    let mut rx = proxy.start().await?;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: "tools/list".to_string(),
        params: None,
    });

    proxy.send(&request).await?;

    if let Some(message) = rx.recv().await {
        println!("{}", serde_json::to_string_pretty(&message)?);
    }

    proxy.stop().await?;
    Ok(())
}
```

## Client config

The main options live on [`NostrClientTransportConfig`](src/transport/client/mod.rs:33):

- `relay_urls`: relays used for direct transport
- `server_pubkey`: target server identity in hex form
- `encryption_mode`: client encryption policy
- `gift_wrap_mode`: preferred gift-wrap kind policy
- `is_stateless`: emulate the initialize response locally
- `timeout`: pending request correlation retention

## Stateless mode

[`is_stateless`](src/transport/client/mod.rs:43) is a major behavior switch.

When enabled, the client can emulate initialize handling locally instead of waiting for a network roundtrip. This behavior is tested in [`tests/conformance_stateless_mode.rs`](tests/conformance_stateless_mode.rs).

Use it when:

- you want faster startup for short-lived clients
- you control the server behavior and know stateless operation is acceptable

Avoid assuming that every server workflow depends only on stateless behavior.

## Behavioral notes

- responses are correlated at the transport level, not just by raw receive order
- the client learns peer capabilities from discovery tags on inbound messages
- encrypted traffic is deduplicated by outer gift-wrap event id before delivery, as covered by [`tests/conformance_dedup.rs`](tests/conformance_dedup.rs)

## When not to use it

Prefer [`client-transport.md`](docs/client-transport.md) when:

- your application is already modeled as an `rmcp` [`ClientHandler`](rust-sdk/crates/rmcp/src/lib.rs:14)
- you want the normal running-client workflow from [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20)
- you want examples that match the rest of the `rmcp` client ecosystem

## rmcp path

If you are building on `rmcp`, use [`serve_client_handler()`](src/proxy/mod.rs:77) instead of manually sending and receiving raw [`JsonRpcMessage`](src/core/types.rs:146) values.

That said, the preferred native documentation path is still [`client-transport.md`](docs/client-transport.md), because it reflects the main architecture: `rmcp` client first, ContextVM transport second.
