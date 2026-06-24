# Rust SDK Docs

This directory contains the in-repo Rust SDK documentation for `contextvm-sdk`.

## Start here

The main mental model is:

1. build an `rmcp` server or client
2. attach a ContextVM transport
3. run MCP over Nostr

For most native Rust applications, the primary entry points are `NostrServerTransport` and `NostrClientTransport`, used together with `rmcp` services via `ServiceExt`.

## Guides

### Native ContextVM applications

- Overview: architecture, API selection, and protocol model
- Native server guide: server setup over Nostr
- Native client guide: client setup over Nostr
- Encryption guide: plaintext, encrypted, and gift-wrap behavior
- Stateless mode guide: client-side initialize emulation and when to use it
- Discovery guide: public discovery helpers and event kinds
- Oversized transfer guide: CEP-22 fragmentation, the three-timer model, and progress-aware request options
- Open-stream guide: CEP-41 streaming responses, the writer and `call_tool_stream` APIs, and the keepalive timer model

### Bridging existing MCP applications

- Gateway guide: expose an existing server-side MCP flow over ContextVM
- Proxy guide: connect to a remote ContextVM server with a lighter client bridge

### Integration notes

- RMCP integration guide: how the optional `rmcp` integration layer fits in
- Transport guide: lower-level transport behavior and direct usage
- Transport modes guide: encryption mode, gift-wrap mode, and stateless mode as one reference

## Documentation goals

The docs here are concise and implementation-driven.

They are derived from the public crate APIs, the `rmcp` service APIs, the repository examples, and the transport and conformance tests in this repository.

They are intended to remain usable on their own, without depending on external documentation pages.
