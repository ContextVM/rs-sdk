# Native Server Guide

Use this path when you are building a native ContextVM server in Rust.

The recommended architecture is:

- define an `rmcp` server handler
- create a `NostrServerTransport`
- attach the transport to the handler with `rmcp`'s `ServiceExt`

This is the same model used by the standard `rmcp` server examples, except the transport is Nostr instead of stdio.

## The high-level shape

In `rmcp`, a native server is normally started with `YourHandler.serve(transport)`.

For ContextVM, the transport becomes `NostrServerTransport`. In the current SDK API, you pass that transport directly to `ServiceExt`; there is no extra adapter step in the public workflow.

## Loading an existing private key

If the server should run under a stable Nostr identity, load the signer from an existing private key with `from_sk()`:

```rust
use contextvm_sdk::signer;

let signer = signer::from_sk("<hex-or-nsec-private-key>")?;
println!("server pubkey: {}", signer.public_key().to_hex());
```

This is the right choice for long-lived servers, announced servers, and deployments where clients must recognize the same public key across restarts.

## Example

```rust
use contextvm_sdk::transport::server::{
    NostrServerTransport, NostrServerTransportConfig,
};
use contextvm_sdk::{EncryptionMode, GiftWrapMode};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::*,
    schemars, tool, tool_handler, tool_router,
};

#[derive(Clone)]
struct DemoServer {
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl DemoServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoParams {
    message: String,
}

#[tool_router]
impl DemoServer {
    #[tool(description = "Echo a message back unchanged")]
    async fn echo(
        &self,
        Parameters(EchoParams { message }): Parameters<EchoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(message)]))
    }
}

#[tool_handler]
impl ServerHandler for DemoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::new("demo-server", "0.1.0"),
            instructions: Some("Try the echo tool".to_string()),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let signer = contextvm_sdk::signer::generate();

    let transport = NostrServerTransport::new(
        signer,
        NostrServerTransportConfig {
            relay_urls: vec!["wss://relay.primal.net".to_string()],
            is_announced_server: true,
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            ..Default::default()
        },
    )
    .await?;

    let service = DemoServer::new().serve(transport).await?;
    service.waiting().await?;
    Ok(())
}
```

This follows the same native service pattern as the repository integration example, but replaces the local duplex transport with `NostrServerTransport`.

## What the transport adds

`NostrServerTransport` is not just a byte stream adapter. It adds ContextVM-specific behavior on top of `rmcp` server semantics:

- Nostr relay connectivity via `NostrServerTransport::new()`
- public announcements via `announce()`
- publication of tools, resources, prompts, and resource templates
- client authorization via `allowed_public_keys` in `NostrServerTransportConfig`
- capability exclusions via `CapabilityExclusion`
- encryption negotiation and response mirroring via `send_response()`
- session management and request routing inside the server event loop

## Configuration fields that matter first

Start with these fields in `NostrServerTransportConfig`:

- `relay_urls`: relays the server will publish to and listen on
- `is_announced_server`: whether the server should participate in public discovery
- `encryption_mode`: plaintext vs encrypted policy
- `gift_wrap_mode`: persistent vs ephemeral wrapping policy
- `allowed_public_keys`: allowlist for private or restricted servers
- `excluded_capabilities`: allow specific methods without fully opening the server

## When to use this instead of the gateway

Use this page's approach when you are writing a new Rust MCP server.

Use the gateway guide when you already have a request loop or existing local MCP service abstraction and want a thinner bridge.

## Behavioral notes

- The `rmcp` server handshake follows the normal `serve_server()` flow.
- `rmcp` accepts pre-init ping and enters the main loop immediately after initialization completes.
- ContextVM response routing depends on request event ids.
- Encryption mirroring and announcement behavior are covered by the integration tests.
