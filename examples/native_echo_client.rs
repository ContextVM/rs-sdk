//! Example: Native rmcp client over ContextVM/Nostr.
//!
//! Usage:
//!   cargo run --example native_echo_client -- <server_pubkey_hex>

use anyhow::{Context, Result};
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::{signer, EncryptionMode, GiftWrapMode};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult},
    ClientHandler, ServiceExt,
};

const RELAY_URL: &str = "wss://relay.contextvm.org";

#[derive(Clone, Default)]
struct EchoClient;

impl ClientHandler for EchoClient {}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("contextvm_sdk=info".parse()?)
                .add_directive("rmcp=warn".parse()?),
        )
        .init();

    let server_pubkey = std::env::args()
        .nth(1)
        .context("Usage: native_echo_client <server_pubkey_hex>")?;

    let signer = signer::generate();
    println!("Native ContextVM echo client starting");
    println!("Relay: {RELAY_URL}");
    println!("Client pubkey: {}", signer.public_key().to_hex());
    println!("Target server pubkey: {server_pubkey}");

    let transport = NostrClientTransport::new(
        signer,
        NostrClientTransportConfig {
            relay_urls: vec![RELAY_URL.to_string()],
            server_pubkey,
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            ..Default::default()
        },
    )
    .await?;

    let client = EchoClient.serve(transport.into_rmcp_transport()).await?;

    let peer_info = client
        .peer_info()
        .context("server did not provide peer info after initialize")?;
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
