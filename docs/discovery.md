# Discovery Guide

The Rust SDK exposes public discovery helpers for finding announced servers and their published capabilities.

These functions query public announcement events so clients can find servers before opening a direct session.

## What is discoverable

The current implementation supports:

- servers via `discover_servers()`
- tools via `discover_tools()`
- resources via `discover_resources()`
- prompts via `discover_prompts()`
- resource templates via `discover_resource_templates()`

When the `rmcp` feature is enabled, typed variants are also available for tools, resources, prompts, and resource templates.

## Minimal example

This follows the repository discovery example.

```rust
use contextvm_sdk::discovery;
use contextvm_sdk::relay::RelayPool;
use contextvm_sdk::signer;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::generate();
    let relays = vec!["wss://relay.damus.io".to_string()];

    let relay_pool = RelayPool::new(keys).await?;
    relay_pool.connect(&relays).await?;
    let client = relay_pool.client();

    let servers = discovery::discover_servers(client, &relays).await?;
    for server in &servers {
        println!("server: {:?}", server.server_info.name);

        let tools = discovery::discover_tools(client, &server.pubkey_parsed, &relays).await?;
        println!("tools: {}", tools.len());
    }

    relay_pool.disconnect().await?;
    Ok(())
}
```

## Discovery event model

The event kinds follow the public announcement model summarized in the repository root README:

- `11316`: server announcement
- `11317`: tools list
- `11318`: resources list
- `11319`: resource templates
- `11320`: prompts list

These event kinds are the SDK's public discovery model for server metadata and advertised MCP capabilities.

## CEP-17 automatic relay resolution

Clients can discover which relays a server uses without hardcoding relay URLs. Set `server_pubkey` to an nprofile (which embeds relay hints) and leave `relay_urls` empty:

```rust
NostrClientTransportConfig::default()
    .with_server_pubkey("nprofile1...") // contains pubkey + relay hints
```

When `start()` is called with empty `relay_urls`, the transport runs a 6-stage resolution pipeline:

1. **Configured relays** -- if `relay_urls` is non-empty, use them directly
2. **nprofile hints** -- relay URLs embedded in the nprofile identity
3. **CEP-17 discovery** -- fetch kind 10002 relay-list events from bootstrap relays
4. **Fallback probing** -- probe `fallback_operational_relay_urls` in parallel with discovery
5. **Sequential fallback** -- if the race winner returned empty, await the other
6. **Bootstrap default** -- fall back to `DEFAULT_BOOTSTRAP_RELAY_URLS` as last resort

The `discovery_relay_urls` and `fallback_operational_relay_urls` config fields customize stages 3-4.

## Important limitations

- discovery is public metadata, not a replacement for direct transport negotiation
- the current helpers fetch and parse latest public lists, but they do not replace direct session learning
- direct session learning still matters for encryption preferences, gift-wrap support, and first-message capability hints on an active connection
