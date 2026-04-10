//! Client-side Nostr transport for ContextVM.
//!
//! Connects to a remote MCP server over Nostr. Sends JSON-RPC requests as
//! kind 25910 events, correlates responses via `e` tag.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::sync::RwLock;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::serializers;
use crate::core::types::*;
use crate::core::validation;
use crate::encryption;
use crate::relay::RelayPool;
use crate::transport::base::BaseTransport;

use crate::util::tracing_setup;

const LOG_TARGET: &str = "contextvm_sdk::transport::client";

/// Configuration for the client transport.
pub struct NostrClientTransportConfig {
    /// Relay URLs to connect to.
    pub relay_urls: Vec<String>,
    /// The server's public key (hex).
    pub server_pubkey: String,
    /// Encryption mode.
    pub encryption_mode: EncryptionMode,
    /// Gift-wrap policy for encrypted messages.
    pub gift_wrap_mode: GiftWrapMode,
    /// Stateless mode: emulate initialize response locally.
    pub is_stateless: bool,
    /// Response timeout (default: 30s).
    pub timeout: Duration,
    /// Optional log file path. Logs always go to stdout and are also appended here when set.
    pub log_file_path: Option<String>,
}

impl Default for NostrClientTransportConfig {
    fn default() -> Self {
        Self {
            relay_urls: vec!["wss://relay.damus.io".to_string()],
            server_pubkey: String::new(),
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            is_stateless: false,
            timeout: Duration::from_secs(30),
            log_file_path: None,
        }
    }
}

/// Client-side Nostr transport for sending MCP requests and receiving responses.
pub struct NostrClientTransport {
    base: BaseTransport,
    config: NostrClientTransportConfig,
    server_pubkey: PublicKey,
    /// Pending request event IDs awaiting responses.
    pending_requests: Arc<RwLock<HashSet<String>>>,
    /// Learned support for server-side ephemeral gift wraps.
    server_supports_ephemeral: Arc<AtomicBool>,
    /// Channel for receiving processed MCP messages from the event loop.
    message_tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
    message_rx: Option<tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>>,
}

impl NostrClientTransport {
    /// Create a new client transport.
    pub async fn new<T>(signer: T, config: NostrClientTransportConfig) -> Result<Self>
    where
        T: IntoNostrSigner,
    {
        tracing_setup::init_tracer(config.log_file_path.as_deref())?;

        let server_pubkey = PublicKey::from_hex(&config.server_pubkey).map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                server_pubkey = %config.server_pubkey,
                "Invalid server pubkey"
            );
            Error::Other(format!("Invalid server pubkey: {error}"))
        })?;

        let relay_pool = Arc::new(RelayPool::new(signer).await.map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                "Failed to initialize relay pool for client transport"
            );
            error
        })?);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.relay_urls.len(),
            stateless = config.is_stateless,
            encryption_mode = ?config.encryption_mode,
            "Created client transport"
        );
        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            config,
            server_pubkey,
            pending_requests: Arc::new(RwLock::new(HashSet::new())),
            server_supports_ephemeral: Arc::new(AtomicBool::new(false)),
            message_tx: tx,
            message_rx: Some(rx),
        })
    }

    /// Connect and start listening for responses.
    pub async fn start(&mut self) -> Result<()> {
        self.base
            .connect(&self.config.relay_urls)
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    "Failed to connect client transport to relays"
                );
                error
            })?;

        let pubkey = self.base.get_public_key().await.map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                "Failed to fetch client transport public key"
            );
            error
        })?;
        tracing::info!(
            target: LOG_TARGET,
            pubkey = %pubkey.to_hex(),
            "Client transport started"
        );

        self.base
            .subscribe_for_pubkey(&pubkey)
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    pubkey = %pubkey.to_hex(),
                    "Failed to subscribe client transport for pubkey"
                );
                error
            })?;

        // Spawn event loop
        let client = self.base.relay_pool.client().clone();
        let pending = self.pending_requests.clone();
        let server_pubkey = self.server_pubkey;
        let tx = self.message_tx.clone();
        let encryption_mode = self.config.encryption_mode;
        let gift_wrap_mode = self.config.gift_wrap_mode;
        let server_supports_ephemeral = self.server_supports_ephemeral.clone();

        tokio::spawn(async move {
            Self::event_loop(
                client,
                pending,
                server_pubkey,
                tx,
                encryption_mode,
                gift_wrap_mode,
                server_supports_ephemeral,
            )
            .await;
        });

        tracing::info!(
            target: LOG_TARGET,
            relay_count = self.config.relay_urls.len(),
            "Client transport event loop spawned"
        );
        Ok(())
    }

    /// Close the transport.
    pub async fn close(&mut self) -> Result<()> {
        self.base.disconnect().await
    }

    /// Send a JSON-RPC message to the server.
    pub async fn send(&self, message: &JsonRpcMessage) -> Result<()> {
        // Stateless mode: emulate initialize response
        if self.config.is_stateless {
            if let JsonRpcMessage::Request(ref req) = message {
                if req.method == "initialize" {
                    self.emulate_initialize_response(&req.id);
                    return Ok(());
                }
            }
            if let JsonRpcMessage::Notification(ref n) = message {
                if n.method == "notifications/initialized" {
                    return Ok(());
                }
            }
        }

        let tags = BaseTransport::create_recipient_tags(&self.server_pubkey);
        let event_id = self
            .base
            .send_mcp_message(
                message,
                &self.server_pubkey,
                CTXVM_MESSAGES_KIND,
                tags,
                None,
                Some(self.choose_outbound_gift_wrap_kind()),
            )
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    server_pubkey = %self.server_pubkey.to_hex(),
                    method = ?message.method(),
                    "Failed to send client message"
                );
                error
            })?;

        if matches!(message, JsonRpcMessage::Request(_)) {
            self.pending_requests
                .write()
                .await
                .insert(event_id.to_hex());
        }

        tracing::debug!(
            target: LOG_TARGET,
            event_id = %event_id.to_hex(),
            method = ?message.method(),
            "Sent client message"
        );
        Ok(())
    }

    /// Take the message receiver for consuming incoming messages.
    pub fn take_message_receiver(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>> {
        self.message_rx.take()
    }

    fn emulate_initialize_response(&self, request_id: &serde_json::Value) {
        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request_id.clone(),
            result: serde_json::json!({
                "protocolVersion": crate::core::constants::mcp_protocol_version(),
                "serverInfo": {
                    "name": "Emulated-Stateless-Server",
                    "version": "1.0.0"
                },
                "capabilities": {
                    "tools": { "listChanged": true },
                    "prompts": { "listChanged": true },
                    "resources": { "subscribe": true, "listChanged": true }
                }
            }),
        });
        let _ = self.message_tx.send(response);
    }

    async fn event_loop(
        client: Arc<Client>,
        pending: Arc<RwLock<HashSet<String>>>,
        server_pubkey: PublicKey,
        tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        server_supports_ephemeral: Arc<AtomicBool>,
    ) {
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                let outer_kind = event.kind.as_u16();
                // Handle gift-wrapped events
                let (actual_event_content, actual_pubkey, e_tag, verified_tags, event_kind) =
                    if outer_kind == GIFT_WRAP_KIND || outer_kind == EPHEMERAL_GIFT_WRAP_KIND {
                        let event_kind = outer_kind;
                        if !Self::accepts_encrypted_event(
                            encryption_mode,
                            gift_wrap_mode,
                            event_kind,
                        ) {
                            let reason = if encryption_mode == EncryptionMode::Disabled {
                                "Skipping encrypted response because client encryption is disabled"
                            } else {
                                "Skipping gift wrap due to CEP-19 policy"
                            };
                            tracing::warn!(
                                target: LOG_TARGET,
                                event_id = %event.id.to_hex(),
                                event_kind = event_kind,
                                configured_mode = ?gift_wrap_mode,
                                reason
                            );
                            continue;
                        }
                        // Single-layer NIP-44 decrypt
                        let signer = match client.signer().await {
                            Ok(s) => s,
                            Err(error) => {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    error = %error,
                                    "Failed to get signer"
                                );
                                continue;
                            }
                        };
                        match encryption::decrypt_gift_wrap_single_layer(&signer, &event).await {
                            Ok(decrypted_json) => {
                                match serde_json::from_str::<Event>(&decrypted_json) {
                                    Ok(inner) => {
                                        if let Err(e) = inner.verify() {
                                            tracing::warn!(
                                                "Inner event signature verification failed: {e}"
                                            );
                                            continue;
                                        }
                                        let e_tag = serializers::get_tag_value(&inner.tags, "e");
                                        (
                                            inner.content,
                                            inner.pubkey,
                                            e_tag,
                                            inner.tags,
                                            Some(event_kind),
                                        )
                                    }
                                    Err(error) => {
                                        tracing::error!(
                                            target: LOG_TARGET,
                                            error = %error,
                                            "Failed to parse inner event"
                                        );
                                        continue;
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    error = %error,
                                    "Failed to decrypt gift wrap"
                                );
                                continue;
                            }
                        }
                    } else {
                        if !Self::accepts_plaintext_event(encryption_mode) {
                            tracing::warn!(
                                target: LOG_TARGET,
                                event_id = %event.id.to_hex(),
                                "Skipping plaintext response because client encryption is required"
                            );
                            continue;
                        }
                        let e_tag = serializers::get_tag_value(&event.tags, "e");
                        (
                            event.content.clone(),
                            event.pubkey,
                            e_tag,
                            event.tags.clone(),
                            None,
                        )
                    };

                // Verify it's from our server
                if actual_pubkey != server_pubkey {
                    tracing::debug!(
                        target: LOG_TARGET,
                        event_pubkey = %actual_pubkey.to_hex(),
                        expected_pubkey = %server_pubkey.to_hex(),
                        "Skipping event from unexpected pubkey"
                    );
                    continue;
                }

                if Self::should_learn_ephemeral_support(
                    actual_pubkey,
                    server_pubkey,
                    event_kind,
                    &verified_tags,
                ) {
                    server_supports_ephemeral.store(true, Ordering::Relaxed);
                }

                // Parse MCP message
                if let Some(mcp_msg) = validation::validate_and_parse(&actual_event_content) {
                    if Self::requires_pending_response_route(&mcp_msg) {
                        let Some(ref correlated_id) = e_tag else {
                            tracing::warn!(
                                target: LOG_TARGET,
                                "Received JSON-RPC response without correlation e tag"
                            );
                            continue;
                        };
                        let is_pending = pending.read().await.contains(correlated_id.as_str());
                        if !is_pending {
                            tracing::warn!(
                                target: LOG_TARGET,
                                correlated_event_id = %correlated_id,
                                "Response for unknown request"
                            );
                            continue;
                        }
                        pending.write().await.remove(correlated_id.as_str());
                    }
                    let _ = tx.send(mcp_msg);
                }
            }
        }
    }

    fn choose_outbound_gift_wrap_kind(&self) -> u16 {
        match self.config.gift_wrap_mode {
            GiftWrapMode::Persistent => GIFT_WRAP_KIND,
            GiftWrapMode::Ephemeral => EPHEMERAL_GIFT_WRAP_KIND,
            GiftWrapMode::Optional => {
                if self.server_supports_ephemeral.load(Ordering::Relaxed) {
                    EPHEMERAL_GIFT_WRAP_KIND
                } else {
                    GIFT_WRAP_KIND
                }
            }
        }
    }

    fn has_support_ephemeral_tag(tags: &Tags) -> bool {
        tags.iter().any(|tag| {
            tag.kind()
                == TagKind::Custom(
                    crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL.into(),
                )
        })
    }

    fn accepts_encrypted_event(
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        event_kind: u16,
    ) -> bool {
        encryption_mode != EncryptionMode::Disabled && gift_wrap_mode.allows_kind(event_kind)
    }

    fn accepts_plaintext_event(encryption_mode: EncryptionMode) -> bool {
        encryption_mode != EncryptionMode::Required
    }

    fn should_learn_ephemeral_support(
        actual_pubkey: PublicKey,
        server_pubkey: PublicKey,
        event_kind: Option<u16>,
        tags: &Tags,
    ) -> bool {
        actual_pubkey == server_pubkey
            && (event_kind == Some(EPHEMERAL_GIFT_WRAP_KIND)
                || Self::has_support_ephemeral_tag(tags))
    }

    fn requires_pending_response_route(message: &JsonRpcMessage) -> bool {
        matches!(
            message,
            JsonRpcMessage::Response(_) | JsonRpcMessage::ErrorResponse(_)
        )
    }

    /// Returns whether the client has learned ephemeral gift-wrap support from the server.
    pub fn server_supports_ephemeral_encryption(&self) -> bool {
        self.server_supports_ephemeral.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = NostrClientTransportConfig::default();
        assert_eq!(config.relay_urls, vec!["wss://relay.damus.io".to_string()]);
        assert!(config.server_pubkey.is_empty());
        assert_eq!(config.encryption_mode, EncryptionMode::Optional);
        assert_eq!(config.gift_wrap_mode, GiftWrapMode::Optional);
        assert!(!config.is_stateless);
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert!(config.log_file_path.is_none());
    }

    #[test]
    fn test_stateless_config() {
        let config = NostrClientTransportConfig {
            is_stateless: true,
            ..Default::default()
        };
        assert!(config.is_stateless);
    }

    #[test]
    fn test_has_support_ephemeral_tag_detects_capability() {
        let tags = Tags::from_list(vec![Tag::custom(
            TagKind::Custom(crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
            Vec::<String>::new(),
        )]);
        assert!(NostrClientTransport::has_support_ephemeral_tag(&tags));
    }

    #[test]
    fn test_accepts_encrypted_event_respects_encryption_and_gift_wrap_policy() {
        assert!(NostrClientTransport::accepts_encrypted_event(
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
            GIFT_WRAP_KIND,
        ));
        assert!(!NostrClientTransport::accepts_encrypted_event(
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            GIFT_WRAP_KIND,
        ));
        assert!(!NostrClientTransport::accepts_encrypted_event(
            EncryptionMode::Optional,
            GiftWrapMode::Persistent,
            EPHEMERAL_GIFT_WRAP_KIND,
        ));
    }

    #[test]
    fn test_accepts_plaintext_event_respects_required_encryption() {
        assert!(NostrClientTransport::accepts_plaintext_event(
            EncryptionMode::Optional
        ));
        assert!(!NostrClientTransport::accepts_plaintext_event(
            EncryptionMode::Required
        ));
    }

    #[test]
    fn test_should_learn_ephemeral_support_requires_matching_server_pubkey() {
        let server_keys = Keys::generate();
        let other_keys = Keys::generate();
        let tags = Tags::from_list(vec![Tag::custom(
            TagKind::Custom(crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
            Vec::<String>::new(),
        )]);

        assert!(!NostrClientTransport::should_learn_ephemeral_support(
            other_keys.public_key(),
            server_keys.public_key(),
            Some(EPHEMERAL_GIFT_WRAP_KIND),
            &tags,
        ));
        assert!(NostrClientTransport::should_learn_ephemeral_support(
            server_keys.public_key(),
            server_keys.public_key(),
            Some(EPHEMERAL_GIFT_WRAP_KIND),
            &tags,
        ));
    }

    #[test]
    fn test_requires_pending_response_route_only_for_jsonrpc_responses() {
        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            result: serde_json::json!({}),
        });
        let error = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            error: JsonRpcError {
                code: -1,
                message: "oops".to_string(),
                data: None,
            },
        });
        let notification = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "payment_required".to_string(),
            params: Some(serde_json::json!({})),
        });

        assert!(NostrClientTransport::requires_pending_response_route(
            &response
        ));
        assert!(NostrClientTransport::requires_pending_response_route(
            &error
        ));
        assert!(!NostrClientTransport::requires_pending_response_route(
            &notification
        ));
    }

    #[test]
    fn test_stateless_emulated_initialize_response_shape() {
        // Verify the emulated response has the expected structure
        let request_id = serde_json::json!(1);
        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request_id.clone(),
            result: serde_json::json!({
                "protocolVersion": crate::core::constants::mcp_protocol_version(),
                "serverInfo": {
                    "name": "Emulated-Stateless-Server",
                    "version": "1.0.0"
                },
                "capabilities": {
                    "tools": { "listChanged": true },
                    "prompts": { "listChanged": true },
                    "resources": { "subscribe": true, "listChanged": true }
                }
            }),
        });
        assert!(response.is_response());
        assert_eq!(response.id(), Some(&serde_json::json!(1)));

        if let JsonRpcMessage::Response(r) = &response {
            assert!(r.result.get("capabilities").is_some());
            assert!(r.result.get("serverInfo").is_some());
            let server_info = r.result.get("serverInfo").unwrap();
            assert_eq!(
                server_info.get("name").unwrap().as_str().unwrap(),
                "Emulated-Stateless-Server"
            );
        }
    }

    #[test]
    fn test_stateless_mode_initialize_request_detection() {
        let init_req = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        });
        assert_eq!(init_req.method(), Some("initialize"));

        let init_notif = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        });
        assert_eq!(init_notif.method(), Some("notifications/initialized"));
    }
}
