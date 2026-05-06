# ContextVM Rust SDK Overview

The Rust SDK implements ContextVM: MCP over Nostr.

In practice, it lets you transport MCP JSON-RPC messages through Nostr events, add server discovery through announcement events, and optionally encrypt direct traffic with NIP-44 plus gift wrapping.

## The main mental model

For native Rust applications, ContextVM is primarily a transport for `rmcp`.

That means the usual shape is:

1. define an `rmcp` server or client
2. create a ContextVM Nostr transport
3. attach the transport with [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20)

This is the same pattern shown by `rmcp` examples such as [`rust-sdk/examples/servers/src/calculator_stdio.rs:20`](rust-sdk/examples/servers/src/calculator_stdio.rs:20) and [`rust-sdk/examples/clients/src/streamable_http.rs:24`](rust-sdk/examples/clients/src/streamable_http.rs:24). The only difference is that this SDK replaces stdio, HTTP, or raw sockets with Nostr transports.

## Choose the right API

Most users should start with one of these entry points:

| Use case | Start with |
|---|---|
| Build a native ContextVM server | [`NostrServerTransport`](src/transport/server/mod.rs:87) + `rmcp` [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20) |
| Build a native ContextVM client | [`NostrClientTransport`](src/transport/client/mod.rs:69) + `rmcp` [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20) |
| Expose an already-existing MCP server on Nostr | [`NostrMCPGateway`](src/gateway/mod.rs:20) |
| Connect to a remote ContextVM server with a simpler bridge | [`NostrMCPProxy`](src/proxy/mod.rs:17) |
| Discover public servers and capabilities | [`discover_servers()`](src/discovery/mod.rs:54) and related helpers |
| Work directly with the optional bridge layer | [`serve_handler()`](src/gateway/mod.rs:88) or [`serve_client_handler()`](src/proxy/mod.rs:77) |

## Architecture

The crate is organized in layers:

- [`core`](src/core/types.rs:1): protocol types, validation, serialization, errors
- [`relay`](src/relay/mod.rs): relay pool abstraction
- [`signer`](src/signer/mod.rs): key generation and signer helpers
- [`encryption`](src/encryption/mod.rs): NIP-44 and gift-wrap helpers
- [`transport`](src/transport/mod.rs:1): native ContextVM client/server transports
- [`gateway`](src/gateway/mod.rs:1): wrapper for exposing an existing MCP server flow on Nostr
- [`proxy`](src/proxy/mod.rs:1): wrapper for connecting to a remote server without the full `rmcp` client model
- [`discovery`](src/discovery/mod.rs:1): announcement and capability discovery

The application-facing `rmcp` layer lives in [`rust-sdk/crates/rmcp/src/lib.rs`](rust-sdk/crates/rmcp/src/lib.rs). Its core extension point is [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20), with server startup in [`serve_server()`](rust-sdk/crates/rmcp/src/service/server.rs:114) and client startup in [`serve_client()`](rust-sdk/crates/rmcp/src/service/client.rs:177).

## Protocol model

ContextVM keeps MCP semantics intact and uses Nostr only as the transport envelope.

- MCP payloads are represented by [`JsonRpcMessage`](src/core/types.rs:146)
- direct plaintext ContextVM traffic uses kind `25910`
- encrypted traffic uses gift-wrap kinds `1059` or `21059`
- public discovery uses kinds `11316` through `11320`
- routing is done with `p` tags and request/response correlation with `e` tags, as reflected in [`README.md`](README.md:36)

## Core types you should know

- [`EncryptionMode`](src/core/types.rs:17): `Optional`, `Required`, `Disabled`
- [`GiftWrapMode`](src/core/types.rs:34): `Optional`, `Ephemeral`, `Persistent`
- [`ServerInfo`](src/core/types.rs:67): announcement metadata
- [`CapabilityExclusion`](src/core/types.rs:269): allowlist bypass rules for specific methods or capabilities

## Typical workflows

### Build a native server

1. generate or load keys with [`signer::generate()`](src/signer/mod.rs:13)
2. configure [`NostrServerTransportConfig`](src/transport/server/mod.rs:36)
3. create [`NostrServerTransport`](src/transport/server/mod.rs:87)
4. attach it to an `rmcp` server with [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20)
5. optionally publish announcements with [`announce()`](src/transport/server/mod.rs:583)

### Build a native client

1. generate or load keys
2. configure [`NostrClientTransportConfig`](src/transport/client/mod.rs:33)
3. create [`NostrClientTransport`](src/transport/client/mod.rs:69)
4. attach it to an `rmcp` client with [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20)

### Bridge an existing server or client

If you are not building a native `rmcp` service directly, use the wrapper layer:

- [`NostrMCPGateway`](src/gateway/mod.rs:20) for server-side bridging
- [`NostrMCPProxy`](src/proxy/mod.rs:17) for client-side bridging

### Discover servers

1. create a [`RelayPool`](src/relay/mod.rs)
2. query [`discover_servers()`](src/discovery/mod.rs:54)
3. fetch public tools/resources/prompts with the discovery helpers in [`src/discovery/mod.rs`](src/discovery/mod.rs)

## What is important in this implementation

The Rust SDK already implements behavior that users should rely on:

- stateless client initialization behavior, covered by [`tests/conformance_stateless_mode.rs`](tests/conformance_stateless_mode.rs)
- announcement publication and deletion, covered by [`tests/transport_integration.rs`](tests/transport_integration.rs)
- encryption negotiation and response mirroring, also covered by [`tests/transport_integration.rs`](tests/transport_integration.rs)
- rmcp conversion and routing flow, covered by [`src/rmcp_transport/pipeline_tests.rs`](src/rmcp_transport/pipeline_tests.rs:1)

Use the task-oriented pages in this directory for those details. Start with [`server-transport.md`](docs/server-transport.md) and [`client-transport.md`](docs/client-transport.md) if you are building native ContextVM applications.
