//! ContextVM protocol constants
//!
//! Event kinds and tag names matching the ContextVM specification.
//! See: <https://contextvm.org>

/// ContextVM messages (ephemeral events, kind 25910)
pub const CTXVM_MESSAGES_KIND: u16 = 25910;

/// Encrypted messages using NIP-59 Gift Wrap (kind 1059)
pub const GIFT_WRAP_KIND: u16 = 1059;

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

/// Nostr tag constants
pub mod tags {
    /// Public key tag
    pub const PUBKEY: &str = "p";

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
}

/// Maximum message size (1MB)
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Kinds that should never be encrypted (public announcements)
pub const UNENCRYPTED_KINDS: &[u16] = &[
    SERVER_ANNOUNCEMENT_KIND,
    TOOLS_LIST_KIND,
    RESOURCES_LIST_KIND,
    RESOURCETEMPLATES_LIST_KIND,
    PROMPTS_LIST_KIND,
];
