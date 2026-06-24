//! Example: Native rmcp echo server over ContextVM/Nostr.
//!
//! Usage:
//!   cargo run --example native_echo_server

use anyhow::Result;
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use contextvm_sdk::{signer, EncryptionMode, GiftWrapMode, ServerInfo};
use rmcp::{
    handler::server::wrapper::Parameters, model::*, schemars, tool, tool_handler, tool_router,
    ServerHandler, ServiceExt,
};

const RELAY_URL: &str = "wss://relay.contextvm.org";

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoParams {
    message: String,
}

#[derive(Clone)]
struct EchoServer {}

impl EchoServer {
    fn new() -> Self {
        Self {}
    }
}

#[tool_router]
impl EchoServer {
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
impl ServerHandler for EchoServer {
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
async fn main() -> Result<()> {
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

    let service = EchoServer::new().serve(transport).await?;
    println!("Server ready. Press Ctrl+C to stop.");
    service.waiting().await?;
    Ok(())
}
