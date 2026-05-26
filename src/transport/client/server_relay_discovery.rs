//! CEP-17 server relay list fetching and operational relay selection.
//!
//! Fetches kind 10002 relay-list metadata events from discovery relays and
//! selects operational relay URLs based on marker precedence (unmarked > read+write).
//! Mirrors the TS SDK's `fetchServerRelayList()` and `selectOperationalRelayUrls()`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;

use crate::core::constants::{tags, RELAY_LIST_METADATA_KIND};
use crate::core::error::Result;
use crate::relay::{RelayPool, RelayPoolTrait};

/// A single entry from a kind 10002 relay list event.
///
/// Mirrors the TS SDK `RelayListEntry` interface.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayListEntry {
    /// The relay URL.
    pub url: String,
    /// Optional marker: `"read"`, `"write"`, or `None` (unmarked).
    pub marker: Option<String>,
}

/// Select operational relay URLs from relay list entries using marker precedence.
///
/// 1. If any entries are unmarked (no marker), return those URLs (deduplicated).
/// 2. Otherwise, return the union of `read` + `write` entries (deduplicated).
/// 3. Empty strings are filtered out in all cases.
///
/// Mirrors the TS SDK `selectOperationalRelayUrls()`.
pub fn select_operational_relay_urls(entries: &[RelayListEntry]) -> Vec<String> {
    // Collect unmarked entries
    let unmarked: Vec<&str> = entries
        .iter()
        .filter(|e| e.marker.is_none() && !e.url.is_empty())
        .map(|e| e.url.as_str())
        .collect();

    if !unmarked.is_empty() {
        return dedup(unmarked);
    }

    // Fall back to read + write union
    let read_write: Vec<&str> = entries
        .iter()
        .filter(|e| {
            !e.url.is_empty() && matches!(e.marker.as_deref(), Some("read") | Some("write"))
        })
        .map(|e| e.url.as_str())
        .collect();

    dedup(read_write)
}

/// Deduplicate URLs preserving first-seen order.
fn dedup(urls: Vec<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    urls.into_iter()
        .filter(|u| seen.insert(*u))
        .map(|u| u.to_string())
        .collect()
}

/// Fetch the server's kind 10002 relay list from discovery relays.
///
/// Creates a temporary relay pool, connects to `relay_urls`, fetches kind 10002
/// events for `server_pubkey`, extracts relay entries from the latest event,
/// and disconnects. Mirrors the TS SDK `fetchServerRelayList()`.
pub async fn fetch_server_relay_list(
    server_pubkey: &PublicKey,
    relay_urls: &[String],
    signer: Arc<dyn NostrSigner>,
    timeout: Duration,
) -> Result<Vec<RelayListEntry>> {
    let pool = RelayPool::new(signer).await?;
    pool.connect(relay_urls).await?;
    let result = fetch_relay_list_from_pool(server_pubkey, &pool, timeout).await;
    let _ = pool.disconnect().await;
    result
}

/// Core relay-list fetch+parse logic operating on an existing pool.
///
/// Separated from [`fetch_server_relay_list`] so tests can inject events via
/// `MockRelayPool` without needing network access.
pub(crate) async fn fetch_relay_list_from_pool(
    server_pubkey: &PublicKey,
    relay_pool: &dyn RelayPoolTrait,
    timeout: Duration,
) -> Result<Vec<RelayListEntry>> {
    let filter = Filter::new()
        .kind(Kind::Custom(RELAY_LIST_METADATA_KIND))
        .author(*server_pubkey);

    let mut events = relay_pool.fetch_events(vec![filter], timeout).await?;

    if events.is_empty() {
        return Ok(vec![]);
    }

    // Sort by created_at descending, take the latest
    events.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    let latest = &events[0];

    // Extract relay entries from "r" tags.
    // NOTE: Empty URLs are filtered here (diverges from TS SDK which keeps them);
    // malformed empty-URL tags don't occur per NIP-65.
    let entries: Vec<RelayListEntry> = latest
        .tags
        .iter()
        .filter_map(|tag| {
            let parts = tag.clone().to_vec();
            if parts.first().map(|s| s.as_str()) != Some(tags::RELAY) {
                return None;
            }
            let url = parts.get(1)?.clone();
            if url.is_empty() {
                return None;
            }
            // NOTE: An empty-string marker becomes Some(""), not None. The TS SDK
            // treats "" as unmarked (JS falsy). In practice NIP-65 tags never
            // carry an empty marker, so this divergence is benign.
            let marker = parts.get(2).cloned();
            Some(RelayListEntry { url, marker })
        })
        .collect();

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::MockRelayPool;

    // ── select_operational_relay_urls tests ───────────────────────

    #[test]
    fn select_all_unmarked_returns_all_urls() {
        let entries = vec![
            RelayListEntry {
                url: "wss://relay1.example.com".to_string(),
                marker: None,
            },
            RelayListEntry {
                url: "wss://relay2.example.com".to_string(),
                marker: None,
            },
        ];
        let result = select_operational_relay_urls(&entries);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"wss://relay1.example.com".to_string()));
        assert!(result.contains(&"wss://relay2.example.com".to_string()));
    }

    #[test]
    fn select_mixed_markers_prefers_unmarked() {
        let entries = vec![
            RelayListEntry {
                url: "wss://unmarked.example.com".to_string(),
                marker: None,
            },
            RelayListEntry {
                url: "wss://read.example.com".to_string(),
                marker: Some("read".to_string()),
            },
            RelayListEntry {
                url: "wss://write.example.com".to_string(),
                marker: Some("write".to_string()),
            },
        ];
        let result = select_operational_relay_urls(&entries);
        assert_eq!(result, vec!["wss://unmarked.example.com"]);
    }

    #[test]
    fn select_only_read_write_returns_union() {
        let entries = vec![
            RelayListEntry {
                url: "wss://read.example.com".to_string(),
                marker: Some("read".to_string()),
            },
            RelayListEntry {
                url: "wss://write.example.com".to_string(),
                marker: Some("write".to_string()),
            },
        ];
        let result = select_operational_relay_urls(&entries);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"wss://read.example.com".to_string()));
        assert!(result.contains(&"wss://write.example.com".to_string()));
    }

    #[test]
    fn select_empty_input_returns_empty() {
        let result = select_operational_relay_urls(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn select_deduplicates_urls() {
        let entries = vec![
            RelayListEntry {
                url: "wss://relay.example.com".to_string(),
                marker: None,
            },
            RelayListEntry {
                url: "wss://relay.example.com".to_string(),
                marker: None,
            },
        ];
        let result = select_operational_relay_urls(&entries);
        assert_eq!(result, vec!["wss://relay.example.com"]);
    }

    #[test]
    fn select_filters_empty_strings() {
        let entries = vec![
            RelayListEntry {
                url: String::new(),
                marker: None,
            },
            RelayListEntry {
                url: "wss://relay.example.com".to_string(),
                marker: None,
            },
        ];
        let result = select_operational_relay_urls(&entries);
        assert_eq!(result, vec!["wss://relay.example.com"]);
    }

    // ── fetch_server_relay_list tests ────────────────────────────

    fn build_relay_list_event(keys: &Keys, tags: Vec<Tag>, created_at: u64) -> Event {
        let builder = EventBuilder::new(Kind::Custom(RELAY_LIST_METADATA_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at));
        builder.sign_with_keys(keys).unwrap()
    }

    #[tokio::test]
    async fn fetch_returns_parsed_entries_from_injected_event() {
        let pool = MockRelayPool::new();
        let server_keys = Keys::generate();

        let tags = vec![
            Tag::custom(
                TagKind::Custom(tags::RELAY.into()),
                vec!["wss://relay1.example.com"],
            ),
            Tag::custom(
                TagKind::Custom(tags::RELAY.into()),
                vec!["wss://relay2.example.com"],
            ),
        ];
        let event = build_relay_list_event(&server_keys, tags, 1000);
        pool.inject_event(event).await;

        let entries =
            fetch_relay_list_from_pool(&server_keys.public_key(), &pool, Duration::from_secs(5))
                .await
                .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].url, "wss://relay1.example.com");
        assert!(entries[0].marker.is_none());
        assert_eq!(entries[1].url, "wss://relay2.example.com");
        assert!(entries[1].marker.is_none());
    }

    #[tokio::test]
    async fn fetch_no_events_returns_empty() {
        let pool = MockRelayPool::new();
        let server_keys = Keys::generate();

        let entries =
            fetch_relay_list_from_pool(&server_keys.public_key(), &pool, Duration::from_secs(5))
                .await
                .unwrap();

        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn fetch_multiple_events_returns_latest() {
        let pool = MockRelayPool::new();
        let server_keys = Keys::generate();

        // Older event
        let old_tags = vec![Tag::custom(
            TagKind::Custom(tags::RELAY.into()),
            vec!["wss://old.example.com"],
        )];
        let old_event = build_relay_list_event(&server_keys, old_tags, 1000);
        pool.inject_event(old_event).await;

        // Newer event
        let new_tags = vec![Tag::custom(
            TagKind::Custom(tags::RELAY.into()),
            vec!["wss://new.example.com"],
        )];
        let new_event = build_relay_list_event(&server_keys, new_tags, 2000);
        pool.inject_event(new_event).await;

        let entries =
            fetch_relay_list_from_pool(&server_keys.public_key(), &pool, Duration::from_secs(5))
                .await
                .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "wss://new.example.com");
    }

    #[tokio::test]
    async fn fetch_extracts_marker_from_third_tag_element() {
        let pool = MockRelayPool::new();
        let server_keys = Keys::generate();

        let tags = vec![
            Tag::custom(
                TagKind::Custom(tags::RELAY.into()),
                vec!["wss://read.example.com", "read"],
            ),
            Tag::custom(
                TagKind::Custom(tags::RELAY.into()),
                vec!["wss://write.example.com", "write"],
            ),
            Tag::custom(
                TagKind::Custom(tags::RELAY.into()),
                vec!["wss://both.example.com"],
            ),
        ];
        let event = build_relay_list_event(&server_keys, tags, 1000);
        pool.inject_event(event).await;

        let entries =
            fetch_relay_list_from_pool(&server_keys.public_key(), &pool, Duration::from_secs(5))
                .await
                .unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            RelayListEntry {
                url: "wss://read.example.com".to_string(),
                marker: Some("read".to_string()),
            }
        );
        assert_eq!(
            entries[1],
            RelayListEntry {
                url: "wss://write.example.com".to_string(),
                marker: Some("write".to_string()),
            }
        );
        assert_eq!(
            entries[2],
            RelayListEntry {
                url: "wss://both.example.com".to_string(),
                marker: None,
            }
        );
    }
}
