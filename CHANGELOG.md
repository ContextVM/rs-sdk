# Changelog

## [Unreleased]

### Changed

- Bumped `nostr-sdk` from `0.43` to `0.44` (pulls core `nostr` `0.44.3`). No source
  changes were required: the breaking removals in the unreleased 0.45 line
  (`NostrSigner`, `TagKind`, `EventBuilder::sign_with_keys`, `TagStandard`)
  are not yet published. The SDK pins `hex` as a direct dependency, so nostr's
  internal `hex` module removal in 0.44.0 is unaffected.
- FFI: bumped `uniffi` from `0.29` to `0.31`. This raises the embedded UniFFI
  contract version (`29` -> `30`), so the generated `contextvm_ffi.py` / Swift /
  Kotlin bindings and the native library must be taken from the same release —
  a mismatch now aborts at import time with the bumped contract id. Updated
  `.github/workflows/ffi.yml` to install `uniffi-bindgen-cli` at tag `v0.31.2`
  and invoke it as `uniffi-bindgen-cli` (renamed from `uniffi-bindgen` in 0.30).

### Added

- `examples/python/`: runnable Python examples using the UniFFI binding — an
  offline install sanity check, server/tool discovery (mirrors `discovery.rs`),
  and a client `tools/list` caller (mirrors `proxy.rs`).

## [0.1.1] - 2026-05-08

### Added

- End-to-end happy-path integration coverage for the full in-memory SDK stack, exercising RMCP handlers through `NostrServerWorker`, `NostrServerTransport`, `MockRelayPool`, `NostrClientTransport`, and the RMCP client without requiring a live network
- New `test-utils` feature for downstream integration tests that need access to `MockRelayPool`
- Public re-export of the relay module so downstream crates can use `MockRelayPool` through the crate root when `test-utils` is enabled

### Fixed

- RMCP stateless CEP-35 requests are now bridged into the RMCP lifecycle correctly by injecting synthetic initialization for first contact, allowing stateless clients to call tools and resources without an explicit `initialize` round-trip
- Corrected crates.io metadata (repository URL, keywords, categories, homepage, documentation)

### Changed

- Enabled the `rmcp` feature by default to make the native RMCP transport integration available out of the box
- Improved public API exports for transport, relay, gateway, and proxy types to simplify downstream usage

## [0.1.0] - 2026-05-07

### Added

- Core transport layer: `NostrClientTransport` and `NostrServerTransport` over NIP-59 gift wraps
- Gateway and Proxy high-level APIs for bridging MCP over Nostr
- Discovery API: `discover_servers`, `discover_tools`, `discover_resources`, `discover_prompts`, `discover_resource_templates`
- CEP-6: server announcement publishing and querying (kinds 11316–11320)
- CEP-19: ephemeral gift wraps (kind 21059) with `GiftWrapMode` negotiation on both client and server
- CEP-35: stateless session discovery, tag composition, and capability learning
- LRU-bounded session store with configurable capacity (default 1000 sessions) and TTL expiry
- Multi-client support in `NostrServerWorker` (removed single-peer barrier)
- Direct rmcp transport adapters via `into_rmcp_transport()` for native `ContextVM` services
- `CancellationToken`-based graceful shutdown on `close()`
- TTL sweep for client and server correlation stores to prevent pending-request leaks
- `MockRelayPool` for deterministic offline testing
- Builder pattern for all transport and worker configuration structs
- Four examples: gateway, proxy, discovery, and rmcp integration test

### Fixed

- Single-peer barrier in RMCP worker rejected concurrent clients (#60)
- Pending-request leak: correlation store entries never expired by TTL (#61)
- Event loop tasks not cancelled on `close()`, causing resource leaks (#63)
- `RecvError::Lagged` killing event loop under high relay throughput (#68)
- Client race condition: responses lost when publish completed before correlation registration (#55)
- Uncorrelated responses (missing `e` tag) forwarded to consumer instead of dropped (#55)
- Non-atomic `send_response` behavior in server transport (#48)
- Unbounded LRU cache initialization with zero capacity (#50)
- Announced servers not sending JSON-RPC `-32000 Unauthorized` error for disallowed clients (#53)
