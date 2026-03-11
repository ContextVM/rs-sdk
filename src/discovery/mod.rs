//! Server discovery for the ContextVM protocol.
//!
//! Discover MCP servers and their capabilities (tools, resources, prompts)
//! published as Nostr events on relays.
//!
//! # Example
//!
//! ```rust,no_run
//! use contextvm_sdk::discovery;
//! use contextvm_sdk::signer;
//!
//! # async fn example() -> contextvm_sdk::Result<()> {
//! let keys = signer::generate();
//! let relay_pool = contextvm_sdk::RelayPool::new(keys).await?;
//! let relays = vec!["wss://relay.damus.io".to_string()];
//! relay_pool.connect(&relays).await?;
//! let client = relay_pool.client();
//!
//! let servers = discovery::discover_servers(client, &relays).await?;
//! for server in &servers {
//!     println!("Found server: {} ({:?})", server.pubkey, server.server_info.name);
//!     let tools = discovery::discover_tools(client, &server.pubkey_parsed, &relays).await?;
//!     println!("  Tools: {:?}", tools);
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::types::ServerInfo;

/// A discovered server announcement.
#[derive(Debug, Clone)]
pub struct ServerAnnouncement {
    /// Server public key (hex).
    pub pubkey: String,
    /// Parsed public key.
    pub pubkey_parsed: PublicKey,
    /// Server information from the announcement content.
    pub server_info: ServerInfo,
    /// The Nostr event ID of the announcement.
    pub event_id: EventId,
    /// When the announcement was created.
    pub created_at: Timestamp,
}

/// Discover MCP servers by fetching kind 11316 announcement events from relays.
pub async fn discover_servers(
    client: &Arc<Client>,
    _relay_urls: &[String],
) -> Result<Vec<ServerAnnouncement>> {
    let filter = Filter::new().kind(Kind::Custom(SERVER_ANNOUNCEMENT_KIND));

    let events = client
        .fetch_events(filter, Duration::from_secs(10))
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;

    let mut announcements = Vec::new();
    for event in events {
        let server_info: ServerInfo =
            serde_json::from_str(&event.content).unwrap_or_default();
        announcements.push(ServerAnnouncement {
            pubkey: event.pubkey.to_hex(),
            pubkey_parsed: event.pubkey,
            server_info,
            event_id: event.id,
            created_at: event.created_at,
        });
    }

    Ok(announcements)
}

/// Discover tools published by a specific server (kind 11317).
pub async fn discover_tools(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    _relay_urls: &[String],
) -> Result<Vec<serde_json::Value>> {
    fetch_list(client, server_pubkey, TOOLS_LIST_KIND, "tools").await
}

/// Discover resources published by a specific server (kind 11318).
pub async fn discover_resources(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    _relay_urls: &[String],
) -> Result<Vec<serde_json::Value>> {
    fetch_list(client, server_pubkey, RESOURCES_LIST_KIND, "resources").await
}

/// Discover prompts published by a specific server (kind 11320).
pub async fn discover_prompts(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    _relay_urls: &[String],
) -> Result<Vec<serde_json::Value>> {
    fetch_list(client, server_pubkey, PROMPTS_LIST_KIND, "prompts").await
}

/// Discover resource templates published by a specific server (kind 11319).
pub async fn discover_resource_templates(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    _relay_urls: &[String],
) -> Result<Vec<serde_json::Value>> {
    fetch_list(
        client,
        server_pubkey,
        RESOURCETEMPLATES_LIST_KIND,
        "resourceTemplates",
    )
    .await
}

// ── Internal ────────────────────────────────────────────────────────

async fn fetch_list(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    kind: u16,
    list_key: &str,
) -> Result<Vec<serde_json::Value>> {
    let filter = Filter::new()
        .kind(Kind::Custom(kind))
        .author(*server_pubkey);

    let events = client
        .fetch_events(filter, Duration::from_secs(10))
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;

    // Take the most recent event
    let event = match events.into_iter().next() {
        Some(e) => e,
        None => return Ok(Vec::new()),
    };

    let parsed: serde_json::Value =
        serde_json::from_str(&event.content).map_err(|e| Error::Other(e.to_string()))?;

    Ok(parsed
        .get(list_key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ServerInfo;

    #[test]
    fn test_server_info_serialization() {
        let info = ServerInfo {
            name: Some("Test Server".to_string()),
            version: Some("1.0.0".to_string()),
            about: Some("A test MCP server".to_string()),
            website: Some("https://example.com".to_string()),
            picture: Some("https://example.com/pic.png".to_string()),
        };

        let json = serde_json::to_string(&info).unwrap();
        let parsed: ServerInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, Some("Test Server".to_string()));
        assert_eq!(parsed.version, Some("1.0.0".to_string()));
        assert_eq!(parsed.about, Some("A test MCP server".to_string()));
        assert_eq!(parsed.website, Some("https://example.com".to_string()));
        assert_eq!(parsed.picture, Some("https://example.com/pic.png".to_string()));
    }

    #[test]
    fn test_server_info_default() {
        let info = ServerInfo::default();
        assert!(info.name.is_none());
        assert!(info.version.is_none());
        assert!(info.about.is_none());
        assert!(info.website.is_none());
        assert!(info.picture.is_none());
    }

    #[test]
    fn test_server_info_partial_serialization() {
        let info = ServerInfo {
            name: Some("Minimal".to_string()),
            ..Default::default()
        };

        let json = serde_json::to_string(&info).unwrap();
        // Optional fields should be skipped
        assert!(!json.contains("version"));
        assert!(!json.contains("about"));
        assert!(json.contains("Minimal"));
    }

    #[test]
    fn test_server_info_deserialization_from_empty() {
        let info: ServerInfo = serde_json::from_str("{}").unwrap();
        assert!(info.name.is_none());
    }

    #[test]
    fn test_server_announcement_struct() {
        let keys = nostr_sdk::Keys::generate();
        let pubkey = keys.public_key();

        let announcement = ServerAnnouncement {
            pubkey: pubkey.to_hex(),
            pubkey_parsed: pubkey,
            server_info: ServerInfo {
                name: Some("Test".to_string()),
                ..Default::default()
            },
            event_id: EventId::from_hex(
                "0000000000000000000000000000000000000000000000000000000000000001",
            ).unwrap(),
            created_at: Timestamp::now(),
        };

        assert_eq!(announcement.pubkey, pubkey.to_hex());
        assert_eq!(announcement.server_info.name, Some("Test".to_string()));
    }
}
