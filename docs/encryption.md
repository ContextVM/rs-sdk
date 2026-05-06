# Encryption Guide

ContextVM encryption in this SDK is controlled by [`EncryptionMode`](src/core/types.rs:17) and [`GiftWrapMode`](src/core/types.rs:34).

The direct transport behavior is aligned with the protocol references in [`contextvm-docs/src/content/docs/spec/ceps/cep-4.md`](contextvm-docs/src/content/docs/spec/ceps/cep-4.md:2) and [`contextvm-docs/src/content/docs/spec/ceps/cep-19.md`](contextvm-docs/src/content/docs/spec/ceps/cep-19.md:2).

## Encryption modes

[`EncryptionMode`](src/core/types.rs:17) has three modes:

- `Optional`: accept both plaintext and encrypted traffic
- `Required`: require encrypted traffic
- `Disabled`: reject encrypted traffic and use plaintext only

These semantics are not only conceptual; they are exercised in [`tests/transport_integration.rs`](tests/transport_integration.rs).

## Gift-wrap modes

[`GiftWrapMode`](src/core/types.rs:34) controls which outer encrypted event kind is used:

- `Optional`: accept and prefer either supported mode
- `Ephemeral`: use kind `21059`
- `Persistent`: use kind `1059`

The helper methods [`allows_kind()`](src/core/types.rs:46) and [`supports_ephemeral()`](src/core/types.rs:55) show the expected policy behavior.

## Practical rules

### Plaintext transport

Plaintext ContextVM messages use kind `25910` and keep the MCP JSON-RPC payload in the event content.

### Encrypted transport

Encrypted ContextVM messages:

1. serialize the MCP payload to JSON
2. encrypt it with NIP-44
3. wrap it as a gift-wrap event using kind `1059` or `21059`

The implementation details live in [`src/encryption/mod.rs`](src/encryption/mod.rs).

## Response mirroring

One important implementation detail is that server responses mirror the client’s inbound encryption format when policy allows it.

This behavior is verified in [`tests/transport_integration.rs`](tests/transport_integration.rs) and is important for interoperable mixed-mode deployments.

## Deduplication

Both client and server transports deduplicate encrypted outer gift-wrap event ids before delivering them.

This is covered by [`tests/conformance_dedup.rs`](tests/conformance_dedup.rs) and by encrypted transport integration tests in [`tests/transport_integration.rs`](tests/transport_integration.rs).

## Example configuration

```rust
use contextvm_sdk::core::types::{EncryptionMode, GiftWrapMode};
use contextvm_sdk::transport::client::NostrClientTransportConfig;

let config = NostrClientTransportConfig {
    relay_urls: vec!["wss://relay.damus.io".to_string()],
    server_pubkey: "<server-hex-pubkey>".to_string(),
    encryption_mode: EncryptionMode::Optional,
    gift_wrap_mode: GiftWrapMode::Optional,
    ..Default::default()
};
```

## Discovery tags

Encryption support is also surfaced through discovery tags and first-message capability learning, consistent with [`contextvm-docs/src/content/docs/spec/ceps/informational/cep-35.md`](contextvm-docs/src/content/docs/spec/ceps/informational/cep-35.md:60).

In practice, this matters for:

- public announcements
- the first direct server-to-client message
- stateless operation
