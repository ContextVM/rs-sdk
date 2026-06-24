# Changelog

## [0.2.0] - 2026-06-24

### Added

- CEP-22: oversized payload transfer for chunking MCP messages that exceed the NIP-44 single-event size limit (~65 KB), using a transport-agnostic framing engine (start/accept/chunk/end/abort frames, SHA-256 digest verification, and out-of-order reassembly), enabled by default and negotiated through the `support_oversized_transfer` capability tag so servers only fragment to clients that advertise support (#88, #89, #91)
- CEP-22: progress-aware request timeouts and an in-flight transfer watchdog, providing per-chunk idle-timeout reset, a max-total transfer cap, and receiver-side reaping of stalled transfers, opt-in via `call_tool_with_options` and `progress_aware_options` (#92)
- CEP-17: multi-stage relay resolution with server identity parsing, relay list (NIP-65) fetching, and `fetch_events`, plus transport integration that resolves a server's preferred relays before connecting (#82, #83)
- CEP-6: expanded server announcements with full `InitializeResult` parsing in `ServerAnnouncement`, auto-publishing on `start()`, relay list publishing, and a tool and resource schema mapping table (#77, #78, #79, #81)
- CEP-23: optional server profile metadata published as a NIP-01 kind 0 event, via a new `ProfileMetadata` type, so clients see a human-friendly identity (#77, #79)
- CEP-41: open-ended streaming - a server tool emits ordered chunks back to a
  client while a request is in flight via `call_tool_stream`; the client
  consumes them as an async `Stream`; the stream supplements the final
  JSON-RPC response rather than replacing it, negotiated through the
  `support_open_stream` capability tag (#97, #98)
- CI: MSRV and feature-matrix checks (#75)
- `examples/python/`: runnable Python examples using the UniFFI binding — an
  offline install sanity check, server/tool discovery (mirrors `discovery.rs`),
  and a client `tools/list` caller (mirrors `proxy.rs`).

### Changed

- Upgraded `rmcp` from 0.16.0 to 1.8 to gain progress-aware request timeouts (#86)
- Raised the minimum supported Rust version (MSRV) from 1.70 to 1.88
- Added `sha2` and `hex` dependencies for CEP-22 payload digests
- Enabled the `missing_docs` lint, closed rustdoc coverage gaps, and added SDK documentation links and a CEP-22 oversized-transfer guide (#67, #73)
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

### Fixed

- `MockRelayPool` live broadcast now respects per-subscription filters instead of echoing every event to every subscriber (#90)
- Made the oversized-transfer e2e timing tests deterministic with virtual paused time and the relay config hermetic, removing CI flakiness and a 30 s real-network discovery hang (#93, #94)

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
