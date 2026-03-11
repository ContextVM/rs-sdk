//! Example: Discover MCP servers and their tools on Nostr relays.

use contextvm_sdk::discovery;
use contextvm_sdk::relay::RelayPool;
use contextvm_sdk::signer;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    tracing_subscriber::fmt::init();

    let keys = signer::generate();
    let relays = vec!["wss://relay.damus.io".to_string()];

    let relay_pool = RelayPool::new(keys).await?;
    relay_pool.connect(&relays).await?;
    let client = relay_pool.client();

    println!("Discovering MCP servers...\n");

    let servers = discovery::discover_servers(client, &relays).await?;

    if servers.is_empty() {
        println!("No servers found.");
        return Ok(());
    }

    for server in &servers {
        println!(
            "Server: {} (pubkey: {}...)",
            server.server_info.name.as_deref().unwrap_or("unnamed"),
            &server.pubkey[..16]
        );
        if let Some(ref about) = server.server_info.about {
            println!("  About: {about}");
        }

        let tools = discovery::discover_tools(client, &server.pubkey_parsed, &relays).await?;
        if !tools.is_empty() {
            println!("  Tools:");
            for tool in &tools {
                let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                let desc = tool
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                println!("    - {name}: {desc}");
            }
        }

        let resources =
            discovery::discover_resources(client, &server.pubkey_parsed, &relays).await?;
        if !resources.is_empty() {
            println!("  Resources: {} found", resources.len());
        }

        let prompts =
            discovery::discover_prompts(client, &server.pubkey_parsed, &relays).await?;
        if !prompts.is_empty() {
            println!("  Prompts: {} found", prompts.len());
        }

        println!();
    }

    relay_pool.disconnect().await?;
    Ok(())
}
