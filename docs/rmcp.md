# RMCP Integration Guide

For native Rust applications, `rmcp` is the main application layer and ContextVM is the transport layer.

The Rust SDK exposes that integration behind the `rmcp` feature, re-exported from [`src/lib.rs`](src/lib.rs:71). The bridge itself lives in [`src/rmcp_transport/mod.rs`](src/rmcp_transport/mod.rs:1).

## Recommended mental model

Use `rmcp` to define your server or client behavior, then attach ContextVM transports.

That mirrors the transport-agnostic `rmcp` model exposed in [`rust-sdk/crates/rmcp/src/lib.rs`](rust-sdk/crates/rmcp/src/lib.rs), especially [`ServiceExt`](rust-sdk/crates/rmcp/src/lib.rs:20), [`serve_server()`](rust-sdk/crates/rmcp/src/lib.rs:24), and [`serve_client()`](rust-sdk/crates/rmcp/src/lib.rs:22).

The native entry points in this SDK are therefore:

- [`NostrServerTransport`](src/transport/server/mod.rs:87) for servers
- [`NostrClientTransport`](src/transport/client/mod.rs:69) for clients

Start with [`server-transport.md`](docs/server-transport.md) and [`client-transport.md`](docs/client-transport.md) for that workflow.

## Server-side integration

Use [`serve_handler()`](src/gateway/mod.rs:88) to serve an `rmcp` server handler directly over ContextVM.

## Client-side integration

Use [`serve_client_handler()`](src/proxy/mod.rs:77) to connect an `rmcp` client handler through the ContextVM client worker.

## Why this still exists as a separate page

The base SDK does not require `rmcp`. The core message model is represented by [`JsonRpcMessage`](src/core/types.rs:146) and related types in [`src/core/types.rs`](src/core/types.rs).

That separation keeps the transport usable as a lower-level protocol layer, but most application authors will want the `rmcp` path.

The wrapper APIs in [`src/gateway/mod.rs`](src/gateway/mod.rs:1) and [`src/proxy/mod.rs`](src/proxy/mod.rs:1) are convenience layers on top of this broader model, not the primary architecture for native apps.

## Behavioral confidence

The conversion pipeline is covered by [`src/rmcp_transport/pipeline_tests.rs`](src/rmcp_transport/pipeline_tests.rs:1), which tests:

- JSON-RPC parsing into internal message types
- internal-to-rmcp conversion
- rmcp-to-internal conversion
- request id preservation through the bridge
- event-id based routing assumptions used by the server worker
