# Native Client Guide

Use this path when you are building a native ContextVM client in Rust.

The recommended architecture is:

- define an `rmcp` client handler or use a lightweight client info object
- create a [`NostrClientTransport`](src/transport/client/mod.rs:69)
- attach the transport with `rmcp`'s [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20)

This follows the same pattern as `rmcp` client examples such as [`rust-sdk/examples/clients/src/streamable_http.rs`](rust-sdk/examples/clients/src/streamable_http.rs), except the transport is ContextVM over Nostr instead of HTTP.

## The high-level shape

In `rmcp`, a client is typically started with `client_info.serve(transport)`, as shown in [`rust-sdk/examples/clients/src/streamable_http.rs:24`](rust-sdk/examples/clients/src/streamable_http.rs:24).

For ContextVM, the transport becomes [`NostrClientTransport`](src/transport/client/mod.rs:69), then you convert it into an rmcp-compatible adapter with [`into_rmcp_transport()`](src/rmcp_transport/transport.rs:40).

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

    let client = DemoClient
        .serve(transport.into_rmcp_transport())
        .await?;

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

This is the ContextVM equivalent of the workflow in [`rust-sdk/examples/clients/src/streamable_http.rs:19`](rust-sdk/examples/clients/src/streamable_http.rs:19) through [`rust-sdk/examples/clients/src/streamable_http.rs:43`](rust-sdk/examples/clients/src/streamable_http.rs:43), using the direct adapter [`NostrClientTransport::into_rmcp_transport()`](src/rmcp_transport/transport.rs:40).

## What the transport adds

[`NostrClientTransport`](src/transport/client/mod.rs:69) adds ContextVM-specific client behavior on top of `rmcp` client semantics:

- relay connection management via [`NostrClientTransport::new()`](src/transport/client/mod.rs:94)
- target server selection through `server_pubkey` in [`NostrClientTransportConfig`](src/transport/client/mod.rs:33)
- request/response correlation via [`send()`](src/transport/client/mod.rs:283)
- server capability learning via [`learn_server_discovery()`](src/transport/client/mod.rs:477)
- optional stateless behavior via `is_stateless` in [`NostrClientTransportConfig`](src/transport/client/mod.rs:33)
- encrypted message reception and gift-wrap deduplication in [`handle_notification()`](src/transport/client/mod.rs:530)

## Configuration fields that matter first

Start with these fields in [`NostrClientTransportConfig`](src/transport/client/mod.rs:33):

- `relay_urls`: relays the client uses to reach the server
- `server_pubkey`: the target server public key
- `encryption_mode`: whether plaintext is allowed
- `gift_wrap_mode`: whether to use persistent or ephemeral wrapping
- `is_stateless`: whether initialize is emulated locally for stateless workflows
- `timeout`: how long request correlation waits for a response

## When to use this instead of the proxy

Use this page's approach when you are writing a new Rust MCP client that should speak ContextVM natively.

Use [`proxy.md`](docs/proxy.md) when you want a simpler message-oriented bridge and do not want the full `rmcp` running client model.

## Behavioral notes

- The client-side `rmcp` handshake is driven by [`serve_client()`](rust-sdk/crates/rmcp/src/service/client.rs:177).
- The initialize request is sent automatically in [`rust-sdk/crates/rmcp/src/service/client.rs:219`](rust-sdk/crates/rmcp/src/service/client.rs:219).
- Stateless initialization behavior in the ContextVM client transport is covered by [`tests/conformance_stateless_mode.rs`](tests/conformance_stateless_mode.rs).
- Capability learning and gift-wrap handling are implemented in [`src/transport/client/mod.rs:477`](src/transport/client/mod.rs:477) and [`src/transport/client/mod.rs:530`](src/transport/client/mod.rs:530).
