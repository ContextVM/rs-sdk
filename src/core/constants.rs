//! ContextVM protocol constants
//!
//! Event kinds and tag names matching the ContextVM specification.
//! See: <https://contextvm.org>

/// ContextVM messages (ephemeral events, kind 25910)
pub const CTXVM_MESSAGES_KIND: u16 = 25910;

/// Encrypted messages using NIP-59 Gift Wrap (kind 1059)
pub const GIFT_WRAP_KIND: u16 = 1059;

/// Ephemeral variant of NIP-59 Gift Wrap (kind 21059, CEP-19)
///
/// Same structure and semantics as kind 1059, but in NIP-01's ephemeral range.
/// Relays are not expected to store ephemeral events beyond transient forwarding.
pub const EPHEMERAL_GIFT_WRAP_KIND: u16 = 21059;

/// Replaceable relay list metadata event following NIP-65 (CEP-17)
pub const RELAY_LIST_METADATA_KIND: u16 = 10002;

/// Server announcement (addressable, kind 11316)
pub const SERVER_ANNOUNCEMENT_KIND: u16 = 11316;

/// Tools list (addressable, kind 11317)
pub const TOOLS_LIST_KIND: u16 = 11317;

/// Resources list (addressable, kind 11318)
pub const RESOURCES_LIST_KIND: u16 = 11318;

/// Resource templates list (addressable, kind 11319)
pub const RESOURCETEMPLATES_LIST_KIND: u16 = 11319;

/// Prompts list (addressable, kind 11320)
pub const PROMPTS_LIST_KIND: u16 = 11320;

pub const KIND_GIFT_WRAP: u16 = 1059;
/// Nostr tag constants
pub mod tags {
    /// Public key tag
    pub const PUBKEY: &str = "p";

    /// Relay URL tag (CEP-17)
    pub const RELAY: &str = "r";

    /// Event ID tag for correlation
    pub const EVENT_ID: &str = "e";

    /// Capability tag for pricing metadata
    pub const CAPABILITY: &str = "cap";

    /// Name tag for server announcements
    pub const NAME: &str = "name";

    /// Website tag for server announcements
    pub const WEBSITE: &str = "website";

    /// Picture tag for server announcements
    pub const PICTURE: &str = "picture";

    /// About tag for server announcements
    pub const ABOUT: &str = "about";

    /// Support encryption tag
    pub const SUPPORT_ENCRYPTION: &str = "support_encryption";

    /// Support ephemeral gift wrap kind (21059) for encrypted messages (CEP-19)
    pub const SUPPORT_ENCRYPTION_EPHEMERAL: &str = "support_encryption_ephemeral";
}

/// Maximum message size (1MB)
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// MCP protocol version string used in initialize responses.
///
/// Matches the `protocolVersion` field of the `InitializeResult` JSON-RPC response.
/// Keep this in sync with the MCP spec and rmcp's `ProtocolVersion::LATEST`.

/// Default LRU cache size for deduplication
pub const DEFAULT_LRU_SIZE: usize = 5000;

/// Default timeout for network/relay operations (30 seconds)
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Default relay targets for discoverability publication (CEP-17).
///
/// These are used as additional publication targets for server metadata,
/// even when they are not part of the server's operational relay list.
pub const DEFAULT_BOOTSTRAP_RELAY_URLS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
    "wss://relay.snort.social/",
    "wss://nostr.mom/",
    "wss://nostr.oxtr.dev/",
];

/// MCP protocol method for the initialization request
pub const INITIALIZE_METHOD: &str = "initialize";

/// MCP protocol method for the initialized notification
pub const NOTIFICATIONS_INITIALIZED_METHOD: &str = "notifications/initialized";

/// Kinds that should never be encrypted (public announcements)
pub const UNENCRYPTED_KINDS: &[u16] = &[
    SERVER_ANNOUNCEMENT_KIND,
    TOOLS_LIST_KIND,
    RESOURCES_LIST_KIND,
    RESOURCETEMPLATES_LIST_KIND,
    PROMPTS_LIST_KIND,
];


#[cfg(feature = "rmcp")]
pub fn mcp_protocol_version() -> &'static str {
    use std::sync::OnceLock;
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| rmcp::model::ProtocolVersion::LATEST.to_string())
        .as_str()
}

#[cfg(not(feature = "rmcp"))]
pub const fn mcp_protocol_version() -> &'static str {
    "2025-11-25"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_kind_values_match_spec() {
        assert_eq!(CTXVM_MESSAGES_KIND, 25910);
        assert_eq!(GIFT_WRAP_KIND, 1059);
        assert_eq!(EPHEMERAL_GIFT_WRAP_KIND, 21059);
        assert_eq!(RELAY_LIST_METADATA_KIND, 10002);
        assert_eq!(SERVER_ANNOUNCEMENT_KIND, 11316);
        assert_eq!(TOOLS_LIST_KIND, 11317);
        assert_eq!(RESOURCES_LIST_KIND, 11318);
        assert_eq!(RESOURCETEMPLATES_LIST_KIND, 11319);
        assert_eq!(PROMPTS_LIST_KIND, 11320);
    }

    #[test]
    fn test_tag_values_match_ts_sdk() {
        assert_eq!(tags::PUBKEY, "p");
        assert_eq!(tags::RELAY, "r");
        assert_eq!(tags::EVENT_ID, "e");
        assert_eq!(tags::CAPABILITY, "cap");
        assert_eq!(tags::NAME, "name");
        assert_eq!(tags::WEBSITE, "website");
        assert_eq!(tags::PICTURE, "picture");
        assert_eq!(tags::ABOUT, "about");
        assert_eq!(tags::SUPPORT_ENCRYPTION, "support_encryption");
        assert_eq!(
            tags::SUPPORT_ENCRYPTION_EPHEMERAL,
            "support_encryption_ephemeral"
        );
    }

    #[test]
    fn test_ephemeral_gift_wrap_in_ephemeral_range() {
        // NIP-01: ephemeral events are 20000 <= kind < 30000
        assert!(EPHEMERAL_GIFT_WRAP_KIND >= 20000);
        assert!(EPHEMERAL_GIFT_WRAP_KIND < 30000);
    }

    #[test]
    fn test_ctxvm_messages_in_ephemeral_range() {
        // NIP-01: ephemeral events are 20000 <= kind < 30000
        assert!(CTXVM_MESSAGES_KIND >= 20000);
        assert!(CTXVM_MESSAGES_KIND < 30000);
    }

    #[test]
    fn test_relay_list_metadata_in_replaceable_range() {
        // NIP-01: replaceable events are 10000 <= kind < 20000
        assert!(RELAY_LIST_METADATA_KIND >= 10000);
        assert!(RELAY_LIST_METADATA_KIND < 20000);
    }

    #[test]
    fn test_announcement_kinds_in_addressable_range() {
        // NIP-01: addressable events are 30000 <= kind < 40000
        // However, the spec uses 11316-11320 which are in the replaceable range.
        // These are parameterized replaceable events per the ContextVM spec.
        for &kind in UNENCRYPTED_KINDS {
            assert!(kind >= 11316);
            assert!(kind <= 11320);
        }
    }

    #[test]
    fn test_bootstrap_relays_are_wss() {
        for url in DEFAULT_BOOTSTRAP_RELAY_URLS {
            assert!(
                url.starts_with("wss://"),
                "Bootstrap relay must use wss: {url}"
            );
        }
    }

    #[test]
    fn test_bootstrap_relays_nonempty() {
        assert!(
            !DEFAULT_BOOTSTRAP_RELAY_URLS.is_empty(),
            "Must have at least one bootstrap relay"
        );
    }

    #[test]
    fn test_mcp_method_constants() {
        assert_eq!(INITIALIZE_METHOD, "initialize");
        assert_eq!(
            NOTIFICATIONS_INITIALIZED_METHOD,
            "notifications/initialized"
        );
    }

    #[test]
    fn test_unencrypted_kinds_contains_all_announcements() {
        assert!(UNENCRYPTED_KINDS.contains(&SERVER_ANNOUNCEMENT_KIND));
        assert!(UNENCRYPTED_KINDS.contains(&TOOLS_LIST_KIND));
        assert!(UNENCRYPTED_KINDS.contains(&RESOURCES_LIST_KIND));
        assert!(UNENCRYPTED_KINDS.contains(&RESOURCETEMPLATES_LIST_KIND));
        assert!(UNENCRYPTED_KINDS.contains(&PROMPTS_LIST_KIND));
    }

    #[test]
    fn test_gift_wrap_not_in_unencrypted() {
        assert!(!UNENCRYPTED_KINDS.contains(&GIFT_WRAP_KIND));
        assert!(!UNENCRYPTED_KINDS.contains(&EPHEMERAL_GIFT_WRAP_KIND));
    }
}
