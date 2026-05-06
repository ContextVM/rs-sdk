# Native Server Guide

Use this path when you are building a native ContextVM server in Rust.

The recommended architecture is:

- define an `rmcp` server handler
- create a [`NostrServerTransport`](src/transport/server/mod.rs:87)
- attach the transport to the handler with `rmcp`'s [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20)

This is the same model used by `rmcp` examples such as [`rust-sdk/examples/servers/src/calculator_stdio.rs`](rust-sdk/examples/servers/src/calculator_stdio.rs), except the transport is Nostr instead of stdio.

## The high-level shape

In `rmcp`, a native server is normally started with `YourHandler.serve(transport)`, as shown in [`rust-sdk/examples/servers/src/calculator_stdio.rs:20`](rust-sdk/examples/servers/src/calculator_stdio.rs:20).

For ContextVM, the transport becomes [`NostrServerTransport`](src/transport/server/mod.rs:87), then you convert it into an rmcp-compatible adapter with [`into_rmcp_transport()`](src/rmcp_transport/transport.rs:16).

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

    let service = DemoServer::new()
        .serve(transport.into_rmcp_transport())
        .await?;
    service.waiting().await?;
    Ok(())
}
```

This follows the same service pattern used in [`examples/rmcp_integration_test.rs:247`](examples/rmcp_integration_test.rs:247), but replaces the local duplex transport with [`NostrServerTransport`](src/transport/server/mod.rs:87) and its direct rmcp adapter [`NostrServerTransport::into_rmcp_transport()`](src/rmcp_transport/transport.rs:16).

## What the transport adds

[`NostrServerTransport`](src/transport/server/mod.rs:87) is not just a byte stream adapter. It adds ContextVM-specific behavior on top of `rmcp` server semantics:

- Nostr relay connectivity via [`NostrServerTransport::new()`](src/transport/server/mod.rs:126)
- public announcements via [`announce()`](src/transport/server/mod.rs:583)
- publication of tools, resources, prompts, and resource templates via [`publish_tools()`](src/transport/server/mod.rs:638) and related methods
- client authorization via `allowed_public_keys` in [`NostrServerTransportConfig`](src/transport/server/mod.rs:36)
- capability exclusions via [`CapabilityExclusion`](src/core/types.rs:269)
- encryption negotiation and response mirroring via [`send_response()`](src/transport/server/mod.rs:350)
- session management and request routing inside [`event_loop()`](src/transport/server/mod.rs:838)

## Configuration fields that matter first

Start with these fields in [`NostrServerTransportConfig`](src/transport/server/mod.rs:36):

- `relay_urls`: relays the server will publish to and listen on
- `is_announced_server`: whether the server should participate in public discovery
- `encryption_mode`: plaintext vs encrypted policy
- `gift_wrap_mode`: persistent vs ephemeral wrapping policy
- `allowed_public_keys`: allowlist for private or restricted servers
- `excluded_capabilities`: allow specific methods without fully opening the server

## When to use this instead of the gateway

Use this page's approach when you are writing a new Rust MCP server.

Use [`gateway.md`](docs/gateway.md) when you already have a request loop or existing local MCP service abstraction and want a thinner bridge.

## Behavioral notes

- The `rmcp` server handshake begins through [`serve_server()`](rust-sdk/crates/rmcp/src/service/server.rs:114).
- `rmcp` accepts pre-init ping and enters the main loop immediately after sending initialize result, as shown in [`rust-sdk/crates/rmcp/src/service/server.rs:170`](rust-sdk/crates/rmcp/src/service/server.rs:170) and [`rust-sdk/crates/rmcp/src/service/server.rs:250`](rust-sdk/crates/rmcp/src/service/server.rs:250).
- ContextVM response routing depends on request event ids and is tested in [`src/rmcp_transport/pipeline_tests.rs:213`](src/rmcp_transport/pipeline_tests.rs:213).
- Encryption mirroring and announcement behavior are covered by [`tests/transport_integration.rs`](tests/transport_integration.rs).
