# Discovery Guide

The Rust SDK exposes public discovery helpers in [`src/discovery/mod.rs`](src/discovery/mod.rs).

These functions query public announcement events so clients can find servers before opening a direct session.

## What is discoverable

The current implementation supports:

- servers via [`discover_servers()`](src/discovery/mod.rs:54)
- tools via [`discover_tools()`](src/discovery/mod.rs:81)
- resources via [`discover_resources()`](src/discovery/mod.rs:90)
- prompts via [`discover_prompts()`](src/discovery/mod.rs:99)
- resource templates via [`discover_resource_templates()`](src/discovery/mod.rs:108)

When the `rmcp` feature is enabled, typed variants are also available for tools, resources, prompts, and resource templates in [`src/discovery/mod.rs`](src/discovery/mod.rs:122).

## Minimal example

This follows [`examples/discovery.rs`](examples/discovery.rs).

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

The event kinds follow the public announcement model summarized in [`README.md`](README.md:38):

- `11316`: server announcement
- `11317`: tools list
- `11318`: resources list
- `11319`: resource templates
- `11320`: prompts list

This aligns with the public discovery model documented in [`contextvm-docs/src/content/docs/spec/ceps/cep-6.md`](contextvm-docs/src/content/docs/spec/ceps/cep-6.md:2).

## Important limitations

- discovery is public metadata, not a replacement for direct transport negotiation
- the current helpers fetch and parse latest public lists, but they do not replace direct session learning
- for stateless/direct capability learning, see the behavior described in [`contextvm-docs/src/content/docs/spec/ceps/informational/cep-35.md`](contextvm-docs/src/content/docs/spec/ceps/informational/cep-35.md:60)
