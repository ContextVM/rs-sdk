//! Example: Native rmcp echo server over ContextVM/Nostr.
//!
//! Usage:
//!   cargo run --example native_echo_server

use anyhow::Result;
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use contextvm_sdk::{signer, EncryptionMode, GiftWrapMode, ServerInfo};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};

const RELAY_URL: &str = "wss://relay.contextvm.org";

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoParams {
    message: String,
}

#[derive(Clone)]
struct EchoServer {
    tool_router: ToolRouter<Self>,
}

impl EchoServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
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
        NostrServerTransportConfig {
            relay_urls: vec![RELAY_URL.to_string()],
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            is_announced_server: false,
            server_info: Some(ServerInfo {
                name: Some("contextvm-native-echo".to_string()),
                about: Some("Native rmcp echo server example".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;

    let service = EchoServer::new().serve(transport).await?;
    println!("Server ready. Press Ctrl+C to stop.");
    service.waiting().await?;
    Ok(())
}
