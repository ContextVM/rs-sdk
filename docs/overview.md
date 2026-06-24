# ContextVM Rust SDK Overview

The Rust SDK implements ContextVM: MCP over Nostr.

In practice, it lets you transport MCP JSON-RPC messages through Nostr events, add server discovery through announcement events, and optionally encrypt direct traffic with NIP-44 plus gift wrapping.

## The main mental model

For native Rust applications, ContextVM is primarily a transport for `rmcp`.

That means the usual shape is:

1. define an `rmcp` server or client
2. create a ContextVM Nostr transport
3. attach the transport with `ServiceExt`

This is the same pattern shown by the `rmcp` server and client examples. The only difference is that this SDK replaces stdio, HTTP, or raw sockets with Nostr transports.

## Choose the right API

Most users should start with one of these entry points:

| Use case | Start with |
|---|---|
| Build a native ContextVM server | `NostrServerTransport` + `rmcp` `ServiceExt` |
| Build a native ContextVM client | `NostrClientTransport` + `rmcp` `ServiceExt` |
| Expose an already-existing MCP server on Nostr | `NostrMCPGateway` |
| Connect to a remote ContextVM server with a simpler bridge | `NostrMCPProxy` |
| Discover public servers and capabilities | `discover_servers()` and related helpers |
| Work directly with the optional bridge layer | `NostrMCPGateway::serve_handler()` or `NostrMCPProxy::serve_client_handler()` |

## Architecture

The crate is organized in layers:

- `core`: protocol types, validation, serialization, errors
- `relay`: relay pool abstraction
- `signer`: key generation and signer helpers
- `encryption`: NIP-44 and gift-wrap helpers
- `transport`: native ContextVM client and server transports
- `gateway`: wrapper for exposing an existing MCP server flow on Nostr
- `proxy`: wrapper for connecting to a remote server without the full `rmcp` client model
- `discovery`: announcement and capability discovery

The application-facing `rmcp` layer provides the `ServiceExt` integration point together with the usual server and client startup flow.

## Protocol model

ContextVM keeps MCP semantics intact and uses Nostr only as the transport envelope.

- MCP payloads are represented by `JsonRpcMessage`
- direct plaintext ContextVM traffic uses kind `25910`
- encrypted traffic uses gift-wrap kinds `1059` (persistent) or `21059` (ephemeral, CEP-19), negotiated by `GiftWrapMode`
- public server discovery uses announcement kinds `11316` through `11320` (CEP-6)
- server relay lists are published as NIP-65 kind `10002` events (CEP-17)
- optional server profile metadata is published as a NIP-01 kind `0` event (CEP-23)
- oversized transfers (CEP-22) and open streams (CEP-41) both ride inside `notifications/progress` frames on kind `25910`, separated by a `cvm.type` discriminant (`oversized-transfer` and `open-stream`)
- routing is done with `p` tags and request/response correlation with `e` tags, as reflected in the repository root README

## Core types you should know

- `EncryptionMode`: `Optional`, `Required`, `Disabled`
- `GiftWrapMode`: `Optional`, `Ephemeral`, `Persistent` (CEP-19 gift-wrap policy: persistent kind `1059` vs ephemeral kind `21059`)
- `contextvm_sdk::ServerInfo`: announcement metadata
- `contextvm_sdk::ServerAnnouncement`: the discovered-server record returned by `discover_servers()` (CEP-6)
- `contextvm_sdk::ProfileMetadata`: optional NIP-01 kind `0` profile metadata for a human-friendly server identity (CEP-23)
- `CapabilityExclusion`: allowlist bypass rules for specific methods or capabilities
- `OpenStreamConfig`: CEP-41 open-stream settings (disabled by default; see the open-stream guide)
- `ToolStreamCall`: the paired live chunk stream and final result returned by `call_tool_stream`

## Typical workflows

### Build a native server

1. generate keys with `signer::generate()` or load an existing private key with `from_sk()`
2. configure `NostrServerTransportConfig`
3. create `NostrServerTransport`
4. attach it to an `rmcp` server with `ServiceExt`
5. optionally publish announcements with `announce()`

### Build a native client

1. generate keys with `signer::generate()` or load an existing private key with `from_sk()`
2. configure `NostrClientTransportConfig`
3. create `NostrClientTransport`
4. attach it to an `rmcp` client with `ServiceExt`

### Bridge an existing server or client

If you are not building a native `rmcp` service directly, use the wrapper layer:

- `NostrMCPGateway` for server-side bridging
- `NostrMCPProxy` for client-side bridging

### Discover servers

1. create a `RelayPool`
2. query `discover_servers()`
3. fetch public tools, resources, and prompts with the discovery helpers

## What is important in this implementation

The Rust SDK already implements behavior that users should rely on:

- stateless client initialization behavior
- announcement publication and deletion
- encryption negotiation and response mirroring
- rmcp conversion and routing flow

Use the task-oriented pages in this directory for those details. Start with the native server and native client guides if you are building ContextVM applications directly.
