# contextvm-sdk

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org)

Rust SDK for the [ContextVM protocol](https://contextvm.org) — **MCP over Nostr**.

A complete implementation enabling [Model Context Protocol](https://modelcontextprotocol.io) (MCP)
servers and clients to communicate over the [Nostr](https://nostr.com) network with decentralized
discovery, cryptographic identity, and optional end-to-end encryption.

## Architecture

```text
┌──────────────────────────────────────────────────────────┐
│                    Your Application                       │
├──────────────┬───────────────┬────────────────────────────┤
│   Gateway    │     Proxy     │        Discovery           │
│  (server →   │  (nostr →     │  (find servers &           │
│    nostr)    │    client)    │   capabilities)            │
├──────────────┴───────────────┴────────────────────────────┤
│                    Transport Layer                         │
│            NostrServerTransport / NostrClientTransport     │
├───────────────────────────────────────────────────────────┤
│  Core          │  Encryption     │  Relay    │  Signer    │
│  (types,       │  (NIP-44,       │  (pool    │  (key      │
│   JSON-RPC,    │   NIP-59        │   mgmt)   │   mgmt)    │
│   validation)  │   gift wrap)    │           │            │
├────────────────┴─────────────────┴───────────┴────────────┤
│                   Nostr Network (relays)                   │
└───────────────────────────────────────────────────────────┘
```

## Protocol

ContextVM maps MCP's JSON-RPC 2.0 messages onto Nostr events:

| Kind    | Name                   | Type        | Description                          |
|---------|------------------------|-------------|--------------------------------------|
| `25910` | ContextVM Messages     | Ephemeral   | MCP request/response/notification    |
| `1059`  | Gift Wrap (NIP-59)     | Regular     | Encrypted MCP messages               |
| `11316` | Server Announcement    | Addressable | Server identity & metadata           |
| `11317` | Tools List             | Addressable | Published tool capabilities          |
| `11318` | Resources List         | Addressable | Published resource capabilities      |
| `11319` | Resource Templates     | Addressable | Published resource template list     |
| `11320` | Prompts List           | Addressable | Published prompt capabilities        |

Messages are routed using Nostr `p` tags (recipient pubkey) and correlated with `e` tags (request event ID).

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
contextvm-sdk = { git = "https://github.com/k0sti/rust-contextvm-sdk" }
```

Or clone and use as a path dependency:

```toml
[dependencies]
contextvm-sdk = { path = "../rust-contextvm-sdk" }
```

## Quick Start

### Gateway — Expose a Local MCP Server via Nostr

```rust,no_run
use contextvm_sdk::gateway::{NostrMCPGateway, GatewayConfig};
use contextvm_sdk::transport::server::NostrServerTransportConfig;
use contextvm_sdk::core::types::{ServerInfo, EncryptionMode};
use contextvm_sdk::signer;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::generate();

    let config = GatewayConfig {
        nostr_config: NostrServerTransportConfig {
            relay_urls: vec!["wss://relay.damus.io".into()],
            encryption_mode: EncryptionMode::Optional,
            server_info: Some(ServerInfo {
                name: Some("My MCP Server".into()),
                about: Some("Tools via Nostr".into()),
                ..Default::default()
            }),
            is_announced_server: true,
            ..Default::default()
        },
    };

    let mut gateway = NostrMCPGateway::new(keys, config).await?;
    let mut requests = gateway.start().await?;
    gateway.announce().await?;

    while let Some(req) = requests.recv().await {
        println!("Request: {:?}", req.message);
        // Process and respond:
        // gateway.send_response(&req.event_id, response).await?;
    }
    Ok(())
}
```

### Proxy — Connect to a Remote MCP Server via Nostr

```rust,no_run
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::transport::client::NostrClientTransportConfig;
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::signer;

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::generate();

    let config = ProxyConfig {
        nostr_config: NostrClientTransportConfig {
            relay_urls: vec!["wss://relay.damus.io".into()],
            server_pubkey: "abc123...server_hex_pubkey".into(),
            encryption_mode: EncryptionMode::Optional,
            ..Default::default()
        },
    };

    let mut proxy = NostrMCPProxy::new(keys, config).await?;
    let mut responses = proxy.start().await?;

    // Send an MCP request
    let request = contextvm_sdk::JsonRpcMessage::Request(contextvm_sdk::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: serde_json::json!(1),
        method: "tools/list".into(),
        params: None,
    });
    proxy.send(&request).await?;

    // Receive response
    if let Some(msg) = responses.recv().await {
        println!("Response: {:?}", msg);
    }
    Ok(())
}
```

### Discovery — Find MCP Servers on Nostr

```rust,no_run
use contextvm_sdk::{discovery, signer, RelayPool};

#[tokio::main]
async fn main() -> contextvm_sdk::Result<()> {
    let keys = signer::generate();
    let pool = RelayPool::new(keys).await?;
    let relays = vec!["wss://relay.damus.io".into()];
    pool.connect(&relays).await?;

    let servers = discovery::discover_servers(pool.client(), &relays).await?;
    for server in &servers {
        println!("Server: {} ({:?})", server.pubkey, server.server_info.name);

        let tools = discovery::discover_tools(pool.client(), &server.pubkey_parsed, &relays).await?;
        println!("  Tools: {}", tools.len());
    }
    Ok(())
}
```

## Module Overview

| Module         | Description                                                    |
|----------------|----------------------------------------------------------------|
| `core`         | Protocol constants, JSON-RPC types, error types, validation    |
| `transport`    | Client/server Nostr transports with event loop and correlation |
| `gateway`      | High-level gateway bridging local MCP servers to Nostr         |
| `proxy`        | High-level proxy connecting to remote MCP servers via Nostr    |
| `discovery`    | Server/capability discovery via addressable Nostr events       |
| `encryption`   | NIP-44 encryption and NIP-59 gift wrapping                     |
| `relay`        | Nostr relay pool management (connect, publish, subscribe)      |
| `signer`       | Key generation and management utilities                        |

## Configuration

### Encryption Modes

| Mode       | Behavior                                                       |
|------------|----------------------------------------------------------------|
| `Optional` | Encrypt responses if the incoming request was encrypted        |
| `Required` | All messages must be encrypted (rejects plaintext)             |
| `Disabled` | No encryption; all messages sent as plaintext kind 25910       |

Encryption uses **NIP-44** for payload encryption and **NIP-59** (Gift Wrap) for
metadata-private delivery. Server announcements (kinds 11316–11320) are always public.

### Server Transport Config

| Field                    | Default               | Description                              |
|--------------------------|-----------------------|------------------------------------------|
| `relay_urls`             | `["wss://relay.damus.io"]` | Nostr relays to connect to          |
| `encryption_mode`        | `Optional`            | Encryption policy                        |
| `server_info`            | `None`                | Server metadata for announcements        |
| `is_announced_server`    | `false`               | Whether to publish announcements (CEP-6) |
| `allowed_public_keys`    | `[]` (allow all)      | Client pubkey allowlist (hex)            |
| `excluded_capabilities`  | `[]`                  | Methods exempt from allowlist            |
| `session_timeout`        | `300s`                | Inactive session expiry                  |

### Client Transport Config

| Field             | Default                    | Description                          |
|-------------------|----------------------------|--------------------------------------|
| `relay_urls`      | `["wss://relay.damus.io"]` | Nostr relays to connect to           |
| `server_pubkey`   | (required)                 | Target server's public key (hex)     |
| `encryption_mode` | `Optional`                 | Encryption policy                    |
| `is_stateless`    | `false`                    | Emulate initialize locally           |
| `timeout`         | `30s`                      | Response timeout                     |

## References

- [ContextVM Specification](https://contextvm.org)
- [ContextVM TypeScript SDK](https://github.com/ContextVM/sdk)
- [Model Context Protocol](https://modelcontextprotocol.io)
- [Nostr Protocol](https://nostr.com)
- [NIP-44 Encryption](https://github.com/nostr-protocol/nips/blob/master/44.md)
- [NIP-59 Gift Wrap](https://github.com/nostr-protocol/nips/blob/master/59.md)

## License

[MIT](LICENSE)
