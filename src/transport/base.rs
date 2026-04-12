//! Base Nostr transport — shared logic for client and server transports.

use nostr_sdk::prelude::*;
use std::sync::Arc;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::serializers;
use crate::core::types::{EncryptionMode, JsonRpcMessage};
use crate::core::validation;
use crate::encryption;
use crate::relay::RelayPool;

const LOG_TARGET: &str = "contextvm_sdk::transport::base";

/// Shared transport logic for both client and server.
///
/// Handles relay connectivity, event signing/publishing, encryption decisions,
/// and MCP message validation. Used internally by [`NostrClientTransport`](super::client::NostrClientTransport)
/// and [`NostrServerTransport`](super::server::NostrServerTransport).
pub struct BaseTransport {
    /// The relay pool for publishing and subscribing to Nostr events.
    pub relay_pool: Arc<RelayPool>,
    /// The encryption policy for outgoing messages.
    pub encryption_mode: EncryptionMode,
    /// Whether the transport is currently connected to relays.
    pub is_connected: bool,
}

impl BaseTransport {
    /// Connect to relays.
    pub async fn connect(&mut self, relay_urls: &[String]) -> Result<()> {
        if self.is_connected {
            return Ok(());
        }
        self.relay_pool.connect(relay_urls).await?;
        self.is_connected = true;
        Ok(())
    }

    /// Disconnect from relays.
    pub async fn disconnect(&mut self) -> Result<()> {
        if !self.is_connected {
            return Ok(());
        }
        self.relay_pool.disconnect().await?;
        self.is_connected = false;
        Ok(())
    }

    /// Get the public key of the signer.
    pub async fn get_public_key(&self) -> Result<PublicKey> {
        self.relay_pool.public_key().await
    }

    /// Subscribe to events targeting a pubkey (both regular and encrypted).
    ///
    /// Uses three filters: one for ephemeral ContextVM messages (kind 25910)
    /// and two for NIP-59 gift wraps (kinds 1059 and 21059).
    pub async fn subscribe_for_pubkey(&self, pubkey: &PublicKey) -> Result<()> {
        let p_tag = pubkey.to_hex();
        let now = Timestamp::now();

        let ephemeral_filter = Filter::new()
            .kind(Kind::Custom(CTXVM_MESSAGES_KIND))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::P), p_tag.clone())
            .since(now);

        let gift_wrap_filter = Filter::new()
            .kind(Kind::Custom(GIFT_WRAP_KIND))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::P), p_tag.clone())
            .since(now);

        let ephemeral_gift_wrap_filter = Filter::new()
            .kind(Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::P), p_tag)
            .since(now);

        self.relay_pool
            .subscribe(vec![
                ephemeral_filter,
                gift_wrap_filter,
                ephemeral_gift_wrap_filter,
            ])
            .await
    }

    /// Convert a Nostr event to an MCP message with validation.
    pub fn convert_event_to_mcp(&self, content: &str) -> Option<JsonRpcMessage> {
        validation::validate_and_parse(content)
    }

    /// Create a signed Nostr event for an MCP message.
    pub async fn create_signed_event(
        &self,
        message: &JsonRpcMessage,
        kind: u16,
        tags: Vec<Tag>,
    ) -> Result<Event> {
        let builder = serializers::mcp_to_nostr_event(message, kind, tags)?;
        self.relay_pool.sign(builder).await
    }

    /// Send an MCP message to a recipient, optionally encrypting.
    ///
    /// Returns the signed MCP event ID.
    /// When encrypted, this is the inner signed event ID.
    pub async fn send_mcp_message(
        &self,
        message: &JsonRpcMessage,
        recipient: &PublicKey,
        kind: u16,
        tags: Vec<Tag>,
        is_encrypted: Option<bool>,
    ) -> Result<EventId> {
        let should_encrypt = self.should_encrypt(kind, is_encrypted);

        let event = self.create_signed_event(message, kind, tags).await?;
        let signed_event_id = event.id;

        if should_encrypt {
            // Single-layer gift wrap: JSON.stringify(signedEvent) → NIP-44 encrypt
            // This matches the JS/TS SDK's encryptMessage(JSON.stringify(event), recipient)
            let event_json =
                serde_json::to_string(&event).map_err(|e| Error::Encryption(e.to_string()))?;
            let signer = self
                .relay_pool
                .client()
                .signer()
                .await
                .map_err(|e| Error::Encryption(e.to_string()))?;
            let gift_wrap_event =
                encryption::gift_wrap_single_layer(&signer, recipient, &event_json).await?;
            self.relay_pool.publish_event(&gift_wrap_event).await?;
            tracing::debug!(
                target: LOG_TARGET,
                signed_event_id = %signed_event_id,
                envelope_id = %gift_wrap_event.id,
                "Sent encrypted MCP message"
            );
        } else {
            self.relay_pool.publish_event(&event).await?;
            tracing::debug!(
                target: LOG_TARGET,
                signed_event_id = %signed_event_id,
                "Sent unencrypted MCP message"
            );
        }

        Ok(signed_event_id)
    }

    /// Determine whether a message should be encrypted.
    pub fn should_encrypt(&self, kind: u16, is_encrypted: Option<bool>) -> bool {
        // Announcement kinds are never encrypted
        if UNENCRYPTED_KINDS.contains(&kind) {
            return false;
        }

        match self.encryption_mode {
            EncryptionMode::Disabled => false,
            EncryptionMode::Required => true,
            EncryptionMode::Optional => is_encrypted.unwrap_or(true),
        }
    }

    /// Create recipient tags for targeting a specific pubkey.
    pub fn create_recipient_tags(pubkey: &PublicKey) -> Vec<Tag> {
        vec![Tag::public_key(*pubkey)]
    }

    /// Create response tags (recipient + correlated event).
    pub fn create_response_tags(pubkey: &PublicKey, event_id: &EventId) -> Vec<Tag> {
        vec![Tag::public_key(*pubkey), Tag::event(*event_id)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::*;

    // Test should_encrypt logic without constructing full BaseTransport
    fn should_encrypt(mode: EncryptionMode, kind: u16, is_encrypted: Option<bool>) -> bool {
        if UNENCRYPTED_KINDS.contains(&kind) {
            return false;
        }
        match mode {
            EncryptionMode::Disabled => false,
            EncryptionMode::Required => true,
            EncryptionMode::Optional => is_encrypted.unwrap_or(true),
        }
    }

    #[test]
    fn test_should_encrypt_disabled_mode() {
        assert!(!should_encrypt(
            EncryptionMode::Disabled,
            CTXVM_MESSAGES_KIND,
            None
        ));
        assert!(!should_encrypt(
            EncryptionMode::Disabled,
            CTXVM_MESSAGES_KIND,
            Some(true)
        ));
        assert!(!should_encrypt(
            EncryptionMode::Disabled,
            CTXVM_MESSAGES_KIND,
            Some(false)
        ));
    }

    #[test]
    fn test_should_encrypt_required_mode() {
        assert!(should_encrypt(
            EncryptionMode::Required,
            CTXVM_MESSAGES_KIND,
            None
        ));
        assert!(should_encrypt(
            EncryptionMode::Required,
            CTXVM_MESSAGES_KIND,
            Some(false)
        ));
        assert!(should_encrypt(
            EncryptionMode::Required,
            CTXVM_MESSAGES_KIND,
            Some(true)
        ));
    }

    #[test]
    fn test_should_encrypt_optional_mode() {
        // Default (None) → true
        assert!(should_encrypt(
            EncryptionMode::Optional,
            CTXVM_MESSAGES_KIND,
            None
        ));
        assert!(should_encrypt(
            EncryptionMode::Optional,
            CTXVM_MESSAGES_KIND,
            Some(true)
        ));
        assert!(!should_encrypt(
            EncryptionMode::Optional,
            CTXVM_MESSAGES_KIND,
            Some(false)
        ));
    }

    #[test]
    fn test_should_encrypt_announcement_kinds_never_encrypted() {
        for &kind in UNENCRYPTED_KINDS {
            assert!(!should_encrypt(EncryptionMode::Required, kind, Some(true)));
            assert!(!should_encrypt(EncryptionMode::Optional, kind, Some(true)));
            assert!(!should_encrypt(EncryptionMode::Disabled, kind, Some(true)));
        }
    }

    #[test]
    fn test_create_recipient_tags() {
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        let tags = BaseTransport::create_recipient_tags(&pubkey);
        assert_eq!(tags.len(), 1);
        let tag_vec = tags[0].clone().to_vec();
        assert_eq!(tag_vec[0], "p");
        assert_eq!(tag_vec[1], pubkey.to_hex());
    }

    #[test]
    fn test_create_response_tags() {
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        // Create a dummy event ID
        let event_id =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let tags = BaseTransport::create_response_tags(&pubkey, &event_id);
        assert_eq!(tags.len(), 2);

        let t0 = tags[0].clone().to_vec();
        assert_eq!(t0[0], "p");
        assert_eq!(t0[1], pubkey.to_hex());

        let t1 = tags[1].clone().to_vec();
        assert_eq!(t1[0], "e");
        assert_eq!(t1[1], event_id.to_hex());
    }

    #[test]
    fn test_convert_event_to_mcp_valid_request() {
        // We can't easily construct BaseTransport without async relay pool,
        // but convert_event_to_mcp just calls validation functions.
        // Test the underlying logic directly.
        let content = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let value: serde_json::Value = serde_json::from_str(content).unwrap();
        let msg = crate::core::validation::validate_message(&value).unwrap();
        assert!(msg.is_request());
        assert_eq!(msg.method(), Some("tools/list"));
    }

    #[test]
    fn test_convert_event_to_mcp_valid_notification() {
        let content = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let value: serde_json::Value = serde_json::from_str(content).unwrap();
        let msg = crate::core::validation::validate_message(&value).unwrap();
        assert!(msg.is_notification());
    }

    #[test]
    fn test_convert_event_to_mcp_valid_response() {
        let content = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let value: serde_json::Value = serde_json::from_str(content).unwrap();
        let msg = crate::core::validation::validate_message(&value).unwrap();
        assert!(msg.is_response());
    }

    #[test]
    fn test_convert_event_to_mcp_invalid_json() {
        let content = "not json at all";
        let result: std::result::Result<serde_json::Value, _> = serde_json::from_str(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_convert_event_to_mcp_invalid_jsonrpc_version() {
        let content = r#"{"jsonrpc":"1.0","id":1,"method":"test"}"#;
        let value: serde_json::Value = serde_json::from_str(content).unwrap();
        assert!(crate::core::validation::validate_message(&value).is_none());
    }

    #[test]
    fn test_convert_event_to_mcp_oversized_message() {
        let big = "x".repeat(MAX_MESSAGE_SIZE + 1);
        assert!(!crate::core::validation::validate_message_size(&big));
    }
}
