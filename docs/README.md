# Rust SDK Docs

This directory contains the in-repo Rust SDK documentation for `contextvm-sdk`.

## Start here

The main mental model is:

1. build an `rmcp` server or client
2. attach a ContextVM transport
3. run MCP over Nostr

For most native Rust applications, the primary entry points are [`NostrServerTransport`](src/transport/server/mod.rs:87) and [`NostrClientTransport`](src/transport/client/mod.rs:69), used together with `rmcp` services via [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20).

## Guides

### Native ContextVM applications

- [`overview.md`](docs/overview.md)
- [`server-transport.md`](docs/server-transport.md)
- [`client-transport.md`](docs/client-transport.md)
- [`encryption.md`](docs/encryption.md)
- [`discovery.md`](docs/discovery.md)

### Bridging existing MCP applications

- [`gateway.md`](docs/gateway.md)
- [`proxy.md`](docs/proxy.md)

### Integration notes

- [`rmcp.md`](docs/rmcp.md)
- [`transports.md`](docs/transports.md)

## Documentation goals

The docs here are concise and implementation-driven.

They are derived from public APIs in [`src/lib.rs`](src/lib.rs:1), `rmcp` service APIs in [`rust-sdk/crates/rmcp/src/lib.rs`](rust-sdk/crates/rmcp/src/lib.rs), example programs such as [`examples/rmcp_integration_test.rs`](examples/rmcp_integration_test.rs), and transport/conformance tests such as [`tests/transport_integration.rs`](tests/transport_integration.rs).

They are meant to be migrated later into [`contextvm-docs/src/content/docs`](contextvm-docs/src/content/docs).
