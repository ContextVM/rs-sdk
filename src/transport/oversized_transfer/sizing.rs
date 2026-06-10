//! CEP-22 published-event sizing helpers.
//!
//! Ports the TypeScript `measurePublishedMcpMessageSize` and
//! `resolveSafeOversizedChunkSize` from `base-nostr-transport.ts`.
//!
//! Each oversized frame is published as an independently signed (and, when
//! encrypted, gift-wrapped) Nostr event. The *published* event is materially
//! larger than its raw chunk payload: the JSON-RPC `notifications/progress`
//! envelope, the 64-byte id / 64-byte pubkey / 128-byte signature, the tags, and
//! — when encrypted — the NIP-44 + base64 gift-wrap expansion all add overhead,
//! and the chunk `data` string is escaped at every JSON layer. Picking a fixed
//! chunk size therefore risks frames much larger than intended. These helpers
//! measure the *real* published size so the sender can choose a per-chunk budget
//! that keeps every frame within a byte ceiling.

use nostr_sdk::prelude::{Kind, PublicKey, Tag};

use crate::core::constants::CTXVM_MESSAGES_KIND;
use crate::core::error::Result;
use crate::core::types::{JsonRpcMessage, JsonRpcNotification};
use crate::transport::base::BaseTransport;

use super::frame::OversizedFrame;

/// Probe `progressToken` used when sizing chunk frames. A UUID-length placeholder
/// so the measured envelope is representative of (and slightly conservative for)
/// real progress tokens, which the sizing signature deliberately does not thread
/// through (cf. TS, which passes the real token — a negligible byte difference).
const SIZING_PROBE_TOKEN: &str = "00000000-0000-0000-0000-000000000000";

/// Probe `progress` slot used when sizing chunk frames (the no-handshake
/// first-chunk slot). The slot only affects size by a digit or two.
const SIZING_PROBE_PROGRESS: u64 = 2;

/// Build the final outbound Nostr event for `frame` — signing, and gift-wrapping
/// when `is_encrypted` — and return its serialized UTF-8 byte length.
///
/// Mirrors TS `measurePublishedMcpMessageSize`: it reuses
/// [`BaseTransport::prepare_mcp_message`] (the rs-sdk equivalent of TS
/// `buildPublishedMcpEvent`) to produce the exact event that would hit the relay,
/// then measures `serde_json::to_string(event).len()` (Rust strings are UTF-8, so
/// `.len()` is the byte length). The caller passes the continuation-frame `tags`
/// so the measurement reflects exactly what the real chunk frames carry — the
/// client's recipient `p`-tag, or the server's response `p`+`e` tags.
pub async fn measure_published_event_size(
    frame: &JsonRpcNotification,
    base: &BaseTransport,
    recipient: &PublicKey,
    tags: &[Tag],
    is_encrypted: bool,
    gift_wrap_kind: Kind,
) -> Result<usize> {
    let message = JsonRpcMessage::Notification(frame.clone());
    let (_event_id, published) = base
        .prepare_mcp_message(
            &message,
            recipient,
            CTXVM_MESSAGES_KIND,
            tags.to_vec(),
            Some(is_encrypted),
            Some(gift_wrap_kind.as_u16()),
        )
        .await?;

    Ok(serde_json::to_string(&published)?.len())
}

/// Binary-search the largest per-chunk payload size in `[1, desired]` whose
/// published frame event stays within `max_event_bytes`.
///
/// Mirrors TS `resolveSafeOversizedChunkSize`: the probe chunk's payload is a run
/// of backslash bytes — the JSON worst case, since a backslash escapes to two
/// bytes at *every* serialization layer — so the chosen budget stays safe for
/// arbitrary real payloads. `tags` are the continuation-frame tags (passed
/// straight to [`measure_published_event_size`]). Always returns at least 1
/// (matching TS, which never returns 0, even when a single-byte payload already
/// overflows the ceiling).
pub async fn resolve_safe_chunk_size(
    desired: usize,
    base: &BaseTransport,
    recipient: &PublicKey,
    tags: &[Tag],
    is_encrypted: bool,
    gift_wrap_kind: Kind,
    max_event_bytes: usize,
) -> Result<usize> {
    let mut low: usize = 1;
    let mut high: usize = desired.max(1);
    let mut best: usize = 1;

    while low <= high {
        let mid = low + (high - low) / 2;
        let probe = OversizedFrame::Chunk {
            data: "\\".repeat(mid),
        }
        .into_progress_notification(SIZING_PROBE_TOKEN, SIZING_PROBE_PROGRESS, None)?;

        // A frame we cannot even build at this size is, by definition, too big and
        // must be searched lower. This is not hypothetical: in the encrypted path a
        // large probe's JSON double-escapes past NIP-44's ~64 KiB plaintext ceiling
        // and `gift_wrap` errors — without this, the very first probe (mid =
        // desired/2 ≈ 24 000 for the default config) would abort the whole resolve.
        let fits = match measure_published_event_size(
            &probe,
            base,
            recipient,
            tags,
            is_encrypted,
            gift_wrap_kind,
        )
        .await
        {
            Ok(size) => size <= max_event_bytes,
            Err(_) => false,
        };

        if fits {
            best = mid;
            low = mid + 1;
        } else {
            // `mid >= 1` always (low starts at 1), so `mid - 1` cannot underflow.
            high = mid - 1;
        }
    }

    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use nostr_sdk::prelude::Keys;

    use crate::core::types::EncryptionMode;
    use crate::relay::mock::MockRelayPool;
    use crate::relay::RelayPoolTrait;

    fn test_base(mode: EncryptionMode) -> BaseTransport {
        BaseTransport {
            relay_pool: Arc::new(MockRelayPool::new()) as Arc<dyn RelayPoolTrait>,
            encryption_mode: mode,
            is_connected: true,
        }
    }

    fn chunk_frame(data_len: usize) -> JsonRpcNotification {
        OversizedFrame::Chunk {
            data: "x".repeat(data_len),
        }
        .into_progress_notification("tok", 2, None)
        .unwrap()
    }

    #[tokio::test]
    async fn measure_grows_with_data_length() {
        let base = test_base(EncryptionMode::Disabled);
        let recipient = Keys::generate().public_key();
        let tags = BaseTransport::create_recipient_tags(&recipient);

        let small = measure_published_event_size(
            &chunk_frame(10),
            &base,
            &recipient,
            &tags,
            false,
            Kind::Custom(crate::core::constants::GIFT_WRAP_KIND),
        )
        .await
        .unwrap();
        let large = measure_published_event_size(
            &chunk_frame(1000),
            &base,
            &recipient,
            &tags,
            false,
            Kind::Custom(crate::core::constants::GIFT_WRAP_KIND),
        )
        .await
        .unwrap();

        assert!(large > small, "larger data must yield a larger event");
        assert!(small > 0);
    }

    #[tokio::test]
    async fn resolve_never_exceeds_desired_and_is_at_least_one() {
        let base = test_base(EncryptionMode::Disabled);
        let recipient = Keys::generate().public_key();
        let tags = BaseTransport::create_recipient_tags(&recipient);

        // A tiny ceiling that even a 1-byte frame overflows still yields 1.
        let clamped = resolve_safe_chunk_size(
            5000,
            &base,
            &recipient,
            &tags,
            false,
            Kind::Custom(crate::core::constants::GIFT_WRAP_KIND),
            10,
        )
        .await
        .unwrap();
        assert_eq!(clamped, 1, "an unsatisfiable ceiling must still return 1");

        // The result is capped at `desired`.
        let capped = resolve_safe_chunk_size(
            8,
            &base,
            &recipient,
            &tags,
            false,
            Kind::Custom(crate::core::constants::GIFT_WRAP_KIND),
            10_000_000,
        )
        .await
        .unwrap();
        assert_eq!(capped, 8, "a generous ceiling must cap at `desired`");
    }

    #[tokio::test]
    async fn resolve_is_monotonic_in_budget() {
        let base = test_base(EncryptionMode::Disabled);
        let recipient = Keys::generate().public_key();
        let tags = BaseTransport::create_recipient_tags(&recipient);
        let gw = Kind::Custom(crate::core::constants::GIFT_WRAP_KIND);

        let small_budget =
            resolve_safe_chunk_size(48_000, &base, &recipient, &tags, false, gw, 4_000)
                .await
                .unwrap();
        let large_budget =
            resolve_safe_chunk_size(48_000, &base, &recipient, &tags, false, gw, 16_000)
                .await
                .unwrap();

        assert!(
            large_budget >= small_budget,
            "a larger byte ceiling must allow an equal-or-larger chunk ({large_budget} >= {small_budget})"
        );
    }

    #[tokio::test]
    async fn encrypted_frames_force_smaller_chunks_than_plaintext() {
        let recipient = Keys::generate().public_key();
        let tags = BaseTransport::create_recipient_tags(&recipient);
        let gw = Kind::Custom(crate::core::constants::GIFT_WRAP_KIND);

        let plain = resolve_safe_chunk_size(
            48_000,
            &test_base(EncryptionMode::Disabled),
            &recipient,
            &tags,
            false,
            gw,
            8_000,
        )
        .await
        .unwrap();
        let encrypted = resolve_safe_chunk_size(
            48_000,
            &test_base(EncryptionMode::Required),
            &recipient,
            &tags,
            true,
            gw,
            8_000,
        )
        .await
        .unwrap();

        assert!(
            encrypted < plain,
            "gift-wrap expansion must shrink the safe chunk ({encrypted} < {plain})"
        );
    }
}
