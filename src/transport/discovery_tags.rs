//! Discovery tag utilities for CEP-35 capability exchange.
//!
//! Ports the TS SDK's `discovery-tags.ts` module. Provides functions to filter,
//! parse, learn, and merge discovery tags on Nostr events exchanged between
//! MCP clients and servers.

use std::collections::HashSet;

use nostr_sdk::prelude::*;

use crate::core::constants::tags;

/// Routing tag names that are excluded from discovery tags.
const NON_DISCOVERY_TAG_NAMES: &[&str] = &["p", "e"];

/// Capability flags learned from inbound peer discovery tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerCapabilities {
    /// Peer supports NIP-44/NIP-59 encrypted messaging.
    pub supports_encryption: bool,
    /// Peer supports ephemeral gift wraps (kind 21059, CEP-19).
    pub supports_ephemeral_encryption: bool,
    /// Peer supports CEP-22 oversized payload transfer.
    pub supports_oversized_transfer: bool,
}

/// Returns `true` when the tag list contains a single-valued tag whose name matches `name`.
///
/// A single-valued tag is a tag array whose only element is the tag name itself,
/// e.g. `["support_encryption"]`.
pub fn has_single_tag(tags: &[Tag], name: &str) -> bool {
    tags.iter().any(|tag| {
        let v = tag.clone().to_vec();
        v.len() == 1 && v[0] == name
    })
}

/// Filters out routing tags (`p`, `e`) and returns cloned discovery tags.
///
/// Mirrors TS SDK `getDiscoveryTags()`.
pub fn get_discovery_tags(tags: &[Tag]) -> Vec<Tag> {
    tags.iter()
        .filter(|tag| {
            let v = (*tag).clone().to_vec();
            match v.first() {
                Some(name) => !NON_DISCOVERY_TAG_NAMES.contains(&name.as_str()),
                None => false,
            }
        })
        .cloned()
        .collect()
}

/// Inspects tags and returns discovered peer capabilities.
///
/// Mirrors TS SDK `learnPeerCapabilities()`.
pub fn learn_peer_capabilities(tags: &[Tag]) -> PeerCapabilities {
    PeerCapabilities {
        supports_encryption: has_single_tag(tags, tags::SUPPORT_ENCRYPTION),
        supports_ephemeral_encryption: has_single_tag(tags, tags::SUPPORT_ENCRYPTION_EPHEMERAL),
        supports_oversized_transfer: has_single_tag(tags, tags::SUPPORT_OVERSIZED_TRANSFER),
    }
}

/// Parsed capability flags together with the raw discovery tags.
#[derive(Debug, Clone)]
pub struct DiscoveredPeerCapabilities {
    /// The filtered discovery tags (routing tags stripped).
    pub discovery_tags: Vec<Tag>,
    /// Parsed capability flags.
    pub capabilities: PeerCapabilities,
}

/// Parses peer discovery tags into normalized capability flags plus the raw
/// discovery tags for storage/forwarding.
///
/// Mirrors TS SDK `parseDiscoveredPeerCapabilities()`.
pub fn parse_discovered_peer_capabilities(tags: &[Tag]) -> DiscoveredPeerCapabilities {
    let discovery_tags = get_discovery_tags(tags);
    let capabilities = learn_peer_capabilities(&discovery_tags);
    DiscoveredPeerCapabilities {
        discovery_tags,
        capabilities,
    }
}

/// Merges incoming discovery tags into the current set, preserving order and
/// deduplicating by full tag content (all elements must match).
///
/// Mirrors TS SDK `mergeDiscoveryTags()`.
pub fn merge_discovery_tags(current: &[Tag], incoming: &[Tag]) -> Vec<Tag> {
    let mut merged: Vec<Tag> = current.to_vec();
    let mut seen: HashSet<Vec<String>> = merged.iter().map(|t| t.clone().to_vec()).collect();

    for tag in incoming {
        let key = tag.clone().to_vec();
        if seen.insert(key) {
            merged.push(tag.clone());
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tag(parts: &[&str]) -> Tag {
        let kind = TagKind::Custom(parts[0].into());
        let values: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        Tag::custom(kind, values)
    }

    fn tag_name(tag: &Tag) -> String {
        tag.clone().to_vec()[0].clone()
    }

    // ── has_single_tag ──────────────────────────────────────────────

    #[test]
    fn has_single_tag_finds_present() {
        let tags = vec![make_tag(&["support_encryption"])];
        assert!(has_single_tag(&tags, "support_encryption"));
    }

    #[test]
    fn has_single_tag_ignores_multi_value() {
        let tags = vec![make_tag(&["support_encryption", "extra"])];
        assert!(!has_single_tag(&tags, "support_encryption"));
    }

    #[test]
    fn has_single_tag_returns_false_when_absent() {
        let tags = vec![make_tag(&["other_tag"])];
        assert!(!has_single_tag(&tags, "support_encryption"));
    }

    #[test]
    fn has_single_tag_empty_tags() {
        assert!(!has_single_tag(&[], "support_encryption"));
    }

    // ── get_discovery_tags ──────────────────────────────────────────

    #[test]
    fn get_discovery_tags_filters_routing_tags() {
        let tags = vec![
            Tag::public_key(Keys::generate().public_key()),
            Tag::event(EventId::all_zeros()),
            make_tag(&["support_encryption"]),
            make_tag(&["name", "My Server"]),
        ];
        let discovery = get_discovery_tags(&tags);
        assert_eq!(discovery.len(), 2);
        assert_eq!(tag_name(&discovery[0]), "support_encryption");
        assert_eq!(tag_name(&discovery[1]), "name");
    }

    #[test]
    fn get_discovery_tags_empty_input() {
        let discovery = get_discovery_tags(&[]);
        assert!(discovery.is_empty());
    }

    #[test]
    fn get_discovery_tags_all_routing() {
        let tags = vec![
            Tag::public_key(Keys::generate().public_key()),
            Tag::event(EventId::all_zeros()),
        ];
        let discovery = get_discovery_tags(&tags);
        assert!(discovery.is_empty());
    }

    #[test]
    fn get_discovery_tags_preserves_order() {
        let tags = vec![
            make_tag(&["about", "hello"]),
            Tag::public_key(Keys::generate().public_key()),
            make_tag(&["website", "https://example.com"]),
            make_tag(&["support_encryption"]),
        ];
        let discovery = get_discovery_tags(&tags);
        assert_eq!(discovery.len(), 3);
        assert_eq!(tag_name(&discovery[0]), "about");
        assert_eq!(tag_name(&discovery[1]), "website");
        assert_eq!(tag_name(&discovery[2]), "support_encryption");
    }

    // ── learn_peer_capabilities ─────────────────────────────────────

    #[test]
    fn learn_peer_capabilities_all_present() {
        let tags = vec![
            make_tag(&["support_encryption"]),
            make_tag(&["support_encryption_ephemeral"]),
            make_tag(&["support_oversized_transfer"]),
        ];
        let caps = learn_peer_capabilities(&tags);
        assert!(caps.supports_encryption);
        assert!(caps.supports_ephemeral_encryption);
        assert!(caps.supports_oversized_transfer);
    }

    #[test]
    fn learn_peer_capabilities_none_present() {
        let tags = vec![make_tag(&["name", "Server"])];
        let caps = learn_peer_capabilities(&tags);
        assert!(!caps.supports_encryption);
        assert!(!caps.supports_ephemeral_encryption);
        assert!(!caps.supports_oversized_transfer);
    }

    #[test]
    fn learn_peer_capabilities_partial() {
        let tags = vec![make_tag(&["support_encryption"])];
        let caps = learn_peer_capabilities(&tags);
        assert!(caps.supports_encryption);
        assert!(!caps.supports_ephemeral_encryption);
        assert!(!caps.supports_oversized_transfer);
    }

    #[test]
    fn learn_peer_capabilities_empty() {
        let caps = learn_peer_capabilities(&[]);
        assert_eq!(caps, PeerCapabilities::default());
    }

    #[test]
    fn learn_peer_capabilities_ignores_multi_value_capability_tags() {
        // Tags with values (e.g. ["support_encryption", "extra"]) are not
        // single-valued and should not be treated as capability flags.
        let tags = vec![
            make_tag(&["support_encryption", "yes"]),
            make_tag(&["support_encryption_ephemeral"]),
        ];
        let caps = learn_peer_capabilities(&tags);
        assert!(!caps.supports_encryption);
        assert!(caps.supports_ephemeral_encryption);
        assert!(!caps.supports_oversized_transfer);
    }

    // ── parse_discovered_peer_capabilities ──────────────────────────

    #[test]
    fn parse_discovered_peer_capabilities_filters_and_parses() {
        let tags = vec![
            Tag::public_key(Keys::generate().public_key()),
            Tag::event(EventId::all_zeros()),
            make_tag(&["support_encryption"]),
            make_tag(&["support_encryption_ephemeral"]),
            make_tag(&["name", "Test Server"]),
        ];
        let result = parse_discovered_peer_capabilities(&tags);

        // Routing tags filtered out
        assert_eq!(result.discovery_tags.len(), 3);

        // Capabilities parsed correctly
        assert!(result.capabilities.supports_encryption);
        assert!(result.capabilities.supports_ephemeral_encryption);
        assert!(!result.capabilities.supports_oversized_transfer);
    }

    #[test]
    fn parse_discovered_peer_capabilities_empty() {
        let result = parse_discovered_peer_capabilities(&[]);
        assert!(result.discovery_tags.is_empty());
        assert_eq!(result.capabilities, PeerCapabilities::default());
    }

    // ── merge_discovery_tags ────────────────────────────────────────

    #[test]
    fn merge_discovery_tags_appends_new() {
        let current = vec![make_tag(&["support_encryption"])];
        let incoming = vec![make_tag(&["name", "Server"])];
        let merged = merge_discovery_tags(&current, &incoming);
        assert_eq!(merged.len(), 2);
        assert_eq!(tag_name(&merged[0]), "support_encryption");
        assert_eq!(tag_name(&merged[1]), "name");
    }

    #[test]
    fn merge_discovery_tags_deduplicates() {
        let tag_a = make_tag(&["support_encryption"]);
        let tag_b = make_tag(&["support_encryption"]);
        let current = vec![tag_a];
        let incoming = vec![tag_b];
        let merged = merge_discovery_tags(&current, &incoming);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn merge_discovery_tags_deduplicates_with_values() {
        let current = vec![make_tag(&["name", "Server"])];
        let incoming = vec![
            make_tag(&["name", "Server"]),       // exact dup
            make_tag(&["name", "Other Server"]), // same tag name, different value — not a dup
        ];
        let merged = merge_discovery_tags(&current, &incoming);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_discovery_tags_preserves_order() {
        let current = vec![make_tag(&["b_tag"]), make_tag(&["a_tag"])];
        let incoming = vec![make_tag(&["c_tag"]), make_tag(&["a_tag"])];
        let merged = merge_discovery_tags(&current, &incoming);
        assert_eq!(merged.len(), 3);
        assert_eq!(tag_name(&merged[0]), "b_tag");
        assert_eq!(tag_name(&merged[1]), "a_tag");
        assert_eq!(tag_name(&merged[2]), "c_tag");
    }

    #[test]
    fn merge_discovery_tags_both_empty() {
        let merged = merge_discovery_tags(&[], &[]);
        assert!(merged.is_empty());
    }

    #[test]
    fn merge_discovery_tags_current_empty() {
        let incoming = vec![make_tag(&["support_encryption"])];
        let merged = merge_discovery_tags(&[], &incoming);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn merge_discovery_tags_incoming_empty() {
        let current = vec![make_tag(&["support_encryption"])];
        let merged = merge_discovery_tags(&current, &[]);
        assert_eq!(merged.len(), 1);
    }

    // ── PeerCapabilities ────────────────────────────────────────────

    #[test]
    fn peer_capabilities_default_all_false() {
        let caps = PeerCapabilities::default();
        assert!(!caps.supports_encryption);
        assert!(!caps.supports_ephemeral_encryption);
        assert!(!caps.supports_oversized_transfer);
    }

    #[test]
    fn peer_capabilities_copy_semantics() {
        let caps = PeerCapabilities {
            supports_encryption: true,
            supports_ephemeral_encryption: true,
            supports_oversized_transfer: false,
        };
        let copy = caps;
        assert_eq!(caps, copy);
    }
}
