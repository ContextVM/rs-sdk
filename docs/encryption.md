# Encryption Guide

ContextVM encryption in this SDK is controlled by `EncryptionMode` and `GiftWrapMode`.

At a high level, direct traffic can be sent as plaintext ContextVM events or as encrypted NIP-44 payloads wrapped in gift-wrap events, depending on the configured policy on both peers.

## Encryption modes

`EncryptionMode` has three modes:

- `Optional`: accept both plaintext and encrypted traffic
- `Required`: require encrypted traffic
- `Disabled`: reject encrypted traffic and use plaintext only

These semantics are not only conceptual; they are also exercised by the transport integration tests.

## Gift-wrap modes

`GiftWrapMode` controls which outer encrypted event kind is used:

- `Optional`: accept and prefer either supported mode
- `Ephemeral`: use kind `21059`
- `Persistent`: use kind `1059`

The helper methods `allows_kind()` and `supports_ephemeral()` show the expected policy behavior.

## Practical rules

### Plaintext transport

Plaintext ContextVM messages use kind `25910` and keep the MCP JSON-RPC payload in the event content.

### Encrypted transport

Encrypted ContextVM messages:

1. serialize the MCP payload to JSON
2. encrypt it with NIP-44
3. wrap it as a gift-wrap event using kind `1059` or `21059`

The implementation details live in the SDK encryption module.

## Response mirroring

One important implementation detail is that server responses mirror the client’s inbound encryption format when policy allows it.

This behavior is verified by the transport integration tests and is important for interoperable mixed-mode deployments.

## Deduplication

Both client and server transports deduplicate encrypted outer gift-wrap event ids before delivering them.

This is covered by the deduplication and encrypted transport integration tests.

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

Encryption support is also surfaced through discovery tags and first-message capability learning.

In practice, this matters for:

- public announcements
- the first direct server-to-client message
- stateless operation
