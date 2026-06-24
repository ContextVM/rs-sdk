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
    handler::server::wrapper::Parameters,
    model::*,
    schemars, tool, tool_handler, tool_router,
};

const RELAY_URL: &str = "wss://relay.contextvm.org";

#[derive(Clone)]
struct DemoServer {}

impl DemoServer {
    fn new() -> Self {
        Self {}
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
        rmcp::model::ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("contextvm-native-echo", "0.1.0")
                    .with_title("ContextVM Native Echo Server")
                    .with_description("Native rmcp echo server over ContextVM/Nostr"),
            )
            .with_instructions("Call the echo tool with a message string")
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
- `open_stream`: CEP-41 open-stream settings; disabled by default, opt in with `with_open_stream(OpenStreamConfig::enabled())`
- `allowed_public_keys`: allowlist for private or restricted servers
- `excluded_capabilities`: allow specific methods without fully opening the server
- `relay_list_urls`: relay URLs advertised in kind 10002 (CEP-17); defaults to `relay_urls`
- `bootstrap_relay_urls`: additional relays for publishing announcements (CEP-6/17); merged with `relay_list_urls`
- `publish_relay_list`: whether to publish kind 10002 relay list metadata; default `true`
- `profile_metadata`: optional profile metadata for kind 0 publication (CEP-23)

## Streaming responses with open-stream (CEP-41)

When open-stream is enabled and a `tools/call` request carries a `progressToken`,
the transport injects an `OpenStreamWriter` into the request extensions before
dispatch. Tool handlers retrieve it with
`ctx.extensions.get::<OpenStreamWriter>()` (imported from
`contextvm_sdk::transport::open_stream`), write chunks with `writer.write(..)`,
and finish with `writer.close()`. The final `CallToolResult` is returned normally
after the stream closes. Open-stream is disabled by default; enable it with
`with_open_stream(OpenStreamConfig::enabled())`. See
[open-stream.md](open-stream.md) for a full example.

## When to use this instead of the gateway

Use this page's approach when you are writing a new Rust MCP server.

Use the gateway guide when you already have a request loop or existing local MCP service abstraction and want a thinner bridge.

## Behavioral notes

- The `rmcp` server handshake follows `ServiceExt::serve()` on the server handler.
- `rmcp` accepts pre-init ping and enters the main loop immediately after initialization completes.
- ContextVM response routing depends on request event ids.
- Encryption mirroring and announcement behavior are covered by the integration tests.
- Announcement publishing is started by the rmcp worker just after `start()` (not by `transport.start()` itself, because it injects synthetic MCP requests that need an rmcp handler to answer). When `is_announced_server` is `true`, the transport publishes the gated announcement events via those synthetic requests: kind 11316 (server announcement) and kinds 11317-11320 (tools, resources, templates, prompts).
- Independently of `is_announced_server`, it also publishes kind 10002 (relay list, when `publish_relay_list` is true (the default) and the advertised relay URLs are non-empty) and kind 0 (profile metadata, when `profile_metadata` is configured).
