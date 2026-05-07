# RMCP Integration Guide

For native Rust applications, `rmcp` is the main application layer and ContextVM is the transport layer.

The Rust SDK exposes that integration behind the `rmcp` feature and re-exports the `rmcp` crate when that feature is enabled. The bridge lives in the SDK's rmcp transport layer.

## Recommended mental model

Use `rmcp` to define your server or client behavior, then attach ContextVM transports.

That mirrors the transport-agnostic `rmcp` model, especially `ServiceExt`, `serve_server()`, and `serve_client()`.

The native entry points in this SDK are therefore:

- `NostrServerTransport` for servers
- `NostrClientTransport` for clients

For that workflow, use the native server and native client guides in this directory.

## Server-side integration

Use `serve_handler()` to serve an `rmcp` server handler directly over ContextVM.

## Client-side integration

Use `serve_client_handler()` to connect an `rmcp` client handler through the ContextVM client worker.

## Why this still exists as a separate page

The base SDK does not require `rmcp`. The core message model is represented by `JsonRpcMessage` and related internal JSON-RPC types.

That separation keeps the transport usable as a lower-level protocol layer, but most application authors will want the `rmcp` path.

The gateway and proxy APIs are convenience layers on top of this broader model, not the primary architecture for native apps.

## Behavioral confidence

The conversion pipeline is covered by the SDK test suite, which tests:

- JSON-RPC parsing into internal message types
- internal-to-rmcp conversion
- rmcp-to-internal conversion
- request id preservation through the bridge
- event-id based routing assumptions used by the server worker
