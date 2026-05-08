# Proxy Guide

`NostrMCPProxy` is the simplest way to talk to a remote ContextVM server from Rust.

It wraps `NostrClientTransport`, gives you a receiver for responses and notifications, and handles transport startup and shutdown.

For native Rust applications, this is usually not the primary path. Most users should build an `rmcp` client and attach `NostrClientTransport` directly, as described in the native client guide.

## When to use it

Use the proxy when:

- you already know the target server pubkey
- you want a lightweight request/response flow
- you do not need low-level transport hooks

Do not start here if you are writing a new native Rust MCP client from scratch.

## Loading an existing private key

Like the native client transport, the proxy can reuse an existing Nostr identity instead of generating a new one. Load the signer with `from_sk()`:

```rust
use contextvm_sdk::signer;

let signer = signer::from_sk("<hex-or-nsec-private-key>")?;
```

Pass that signer to `NostrMCPProxy::new()` exactly as you would pass a freshly generated keypair.

## Minimal example

This follows the repository proxy example.

```rust
use contextvm_sdk::core::types::{JsonRpcMessage, JsonRpcRequest};
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::from_sk("<hex-or-nsec-private-key>")?;

    let nostr_config = NostrClientTransportConfig::default()
        .with_relay_urls(vec!["wss://relay.damus.io".to_string()])
        .with_server_pubkey("<server-hex-pubkey>");

    let config = ProxyConfig::new(nostr_config);

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

The main options live on `NostrClientTransportConfig`:

- `relay_urls`: relays used for direct transport
- `server_pubkey`: target server identity in hex form
- `encryption_mode`: client encryption policy
- `gift_wrap_mode`: preferred gift-wrap kind policy
- `is_stateless`: emulate the initialize response locally
- `timeout`: pending request correlation retention

## Stateless mode

`is_stateless` is a major behavior switch.

When enabled, the client can emulate initialize handling locally instead of waiting for a network roundtrip. This behavior is covered by the conformance tests.

Use it when:

- you want faster startup for short-lived clients
- you control the server behavior and know stateless operation is acceptable

Avoid assuming that every server workflow depends only on stateless behavior.

## Behavioral notes

- responses are correlated at the transport level, not just by raw receive order
- the client learns peer capabilities from discovery tags on inbound messages
- encrypted traffic is deduplicated by outer gift-wrap event id before delivery

## When not to use it

Prefer the native client transport path when:

- your application is already modeled as an `rmcp` `ClientHandler`
- you want the normal running-client workflow from `ServiceExt`
- you want examples that match the rest of the `rmcp` client ecosystem

## rmcp path

If you are building on `rmcp`, use the associated function `NostrMCPProxy::serve_client_handler()` instead of manually sending and receiving raw `JsonRpcMessage` values.

That said, the preferred native architecture is still `rmcp` client first and ContextVM transport second.
