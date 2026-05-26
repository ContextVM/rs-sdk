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
use contextvm_sdk::{signer, EncryptionMode, GiftWrapMode, ServerInfo};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
};

const RELAY_URL: &str = "wss://relay.contextvm.org";

#[derive(Clone)]
struct DemoServer {
    tool_router: ToolRouter<Self>,
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
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Echo: {message}"
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for DemoServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "contextvm-native-echo".to_string(),
                title: Some("ContextVM Native Echo Server".to_string()),
                version: "0.1.0".to_string(),
                description: Some("Native rmcp echo server over ContextVM/Nostr".to_string()),
                icons: None,
                website_url: None,
            },
            instructions: Some("Call the echo tool with a message string".to_string()),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("contextvm_sdk=info".parse()?)
                .add_directive("rmcp=warn".parse()?),
        )
        .init();

    let signer = signer::generate();
    let pubkey = signer.public_key().to_hex();

    println!("Native ContextVM echo server starting");
    println!("Relay: {RELAY_URL}");
    println!("Server pubkey: {pubkey}");

    let transport = NostrServerTransport::new(
        signer,
        NostrServerTransportConfig::default()
            .with_relay_urls(vec![RELAY_URL.to_string()])
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional)
            .with_announced_server(false)
            .with_server_info(
                ServerInfo::default()
                    .with_name("contextvm-native-echo".to_string())
                    .with_about("Native rmcp echo server example".to_string()),
            ),
    )
    .await?;

    let service = DemoServer::new().serve(transport).await?;
    println!("Server ready. Press Ctrl+C to stop.");
    service.waiting().await?;
    Ok(())
}
```

This follows the same flow as the repository's native echo server example.

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
- `relay_list_urls`: relay URLs advertised in kind 10002 (CEP-17); defaults to `relay_urls`
- `bootstrap_relay_urls`: additional relays for publishing announcements (CEP-6/17); merged with `relay_list_urls`
- `publish_relay_list`: whether to publish kind 10002 relay list metadata; default `true`
- `profile_metadata`: optional profile metadata for kind 0 publication (CEP-23)

## When to use this instead of the gateway

Use this page's approach when you are writing a new Rust MCP server.

Use the gateway guide when you already have a request loop or existing local MCP service abstraction and want a thinner bridge.

## Behavioral notes

- The `rmcp` server handshake follows `ServiceExt::serve()` on the server handler.
- `rmcp` accepts pre-init ping and enters the main loop immediately after initialization completes.
- ContextVM response routing depends on request event ids.
- Encryption mirroring and announcement behavior are covered by the integration tests.
- When `is_announced_server` is `true`, the transport auto-publishes all announcement events on `start()` via synthetic MCP requests: kind 11316 (server announcement), kinds 11317-11320 (tools, resources, templates, prompts), kind 10002 (relay list), and kind 0 (profile metadata if configured).
