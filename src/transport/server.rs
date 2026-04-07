//! Server-side Nostr transport for ContextVM.
//!
//! Listens for incoming MCP requests from clients over Nostr, manages multi-client
//! sessions, handles request/response correlation, and optionally publishes
//! server announcements.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::sync::RwLock;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::types::*;
use crate::core::validation;
use crate::encryption;
use crate::relay::RelayPool;
use crate::transport::base::BaseTransport;

use crate::util::tracing_setup;

const LOG_TARGET: &str = "contextvm_sdk::transport::server";

/// Configuration for the server transport.
pub struct NostrServerTransportConfig {
    /// Relay URLs to connect to.
    pub relay_urls: Vec<String>,
    /// Encryption mode.
    pub encryption_mode: EncryptionMode,
    /// Server information for announcements.
    pub server_info: Option<ServerInfo>,
    /// Whether this server publishes public announcements (CEP-6).
    pub is_announced_server: bool,
    /// Allowed client public keys (hex). Empty = allow all.
    pub allowed_public_keys: Vec<String>,
    /// Capabilities excluded from pubkey whitelisting.
    pub excluded_capabilities: Vec<CapabilityExclusion>,
    /// Session cleanup interval (default: 60s).
    pub cleanup_interval: Duration,
    /// Session timeout (default: 300s).
    pub session_timeout: Duration,
    /// Optional log file path. Logs always go to stdout and are also appended here when set.
    pub log_file_path: Option<String>,
}

impl Default for NostrServerTransportConfig {
    fn default() -> Self {
        Self {
            relay_urls: vec!["wss://relay.damus.io".to_string()],
            encryption_mode: EncryptionMode::Optional,
            server_info: None,
            is_announced_server: false,
            allowed_public_keys: Vec::new(),
            excluded_capabilities: Vec::new(),
            cleanup_interval: Duration::from_secs(60),
            session_timeout: Duration::from_secs(300),
            log_file_path: None,
        }
    }
}

/// Server-side Nostr transport — receives MCP requests and sends responses.
pub struct NostrServerTransport {
    base: BaseTransport,
    config: NostrServerTransportConfig,
    /// Client sessions: client_pubkey_hex → ClientSession
    sessions: Arc<RwLock<HashMap<String, ClientSession>>>,
    /// Reverse lookup: event_id → client_pubkey_hex
    event_to_client: Arc<RwLock<HashMap<String, String>>>,
    /// Channel for incoming MCP messages (consumed by the MCP server).
    message_tx: tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
    message_rx: Option<tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>>,
}

/// An incoming MCP request with metadata for routing the response.
#[derive(Debug)]
pub struct IncomingRequest {
    /// The parsed MCP message.
    pub message: JsonRpcMessage,
    /// The client's public key (hex).
    pub client_pubkey: String,
    /// The Nostr event ID (for response correlation).
    pub event_id: String,
    /// Whether the original message was encrypted.
    pub is_encrypted: bool,
}

impl NostrServerTransport {
    /// Create a new server transport.
    pub async fn new<T>(signer: T, config: NostrServerTransportConfig) -> Result<Self>
    where
        T: IntoNostrSigner,
    {
        tracing_setup::init_tracer(config.log_file_path.as_deref())?;

        let relay_pool = Arc::new(RelayPool::new(signer).await.map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                "Failed to initialize relay pool for server transport"
            );
            error
        })?);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.relay_urls.len(),
            announced = config.is_announced_server,
            encryption_mode = ?config.encryption_mode,
            "Created server transport"
        );
        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            config,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_to_client: Arc::new(RwLock::new(HashMap::new())),
            message_tx: tx,
            message_rx: Some(rx),
        })
    }

    /// Start listening for incoming requests.
    pub async fn start(&mut self) -> Result<()> {
        self.base
            .connect(&self.config.relay_urls)
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    "Failed to connect server transport to relays"
                );
                error
            })?;

        let pubkey = self.base.get_public_key().await.map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                "Failed to fetch server transport public key"
            );
            error
        })?;
        tracing::info!(
            target: LOG_TARGET,
            pubkey = %pubkey.to_hex(),
            "Server transport started"
        );

        self.base
            .subscribe_for_pubkey(&pubkey)
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    pubkey = %pubkey.to_hex(),
                    "Failed to subscribe server transport for pubkey"
                );
                error
            })?;

        // Spawn event loop
        let client = self.base.relay_pool.client().clone();
        let sessions = self.sessions.clone();
        let event_to_client = self.event_to_client.clone();
        let tx = self.message_tx.clone();
        let allowed = self.config.allowed_public_keys.clone();
        let excluded = self.config.excluded_capabilities.clone();
        let encryption_mode = self.config.encryption_mode;

        tokio::spawn(async move {
            Self::event_loop(
                client,
                sessions,
                event_to_client,
                tx,
                allowed,
                excluded,
                encryption_mode,
            )
            .await;
        });

        // Spawn session cleanup
        let sessions_cleanup = self.sessions.clone();
        let event_to_client_cleanup = self.event_to_client.clone();
        let cleanup_interval = self.config.cleanup_interval;
        let session_timeout = self.config.session_timeout;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_interval);
            loop {
                interval.tick().await;
                let cleaned = Self::cleanup_sessions(
                    &sessions_cleanup,
                    &event_to_client_cleanup,
                    session_timeout,
                )
                .await;
                if cleaned > 0 {
                    tracing::info!(
                        target: LOG_TARGET,
                        cleaned_sessions = cleaned,
                        "Cleaned up inactive sessions"
                    );
                }
            }
        });

        tracing::info!(
            target: LOG_TARGET,
            relay_count = self.config.relay_urls.len(),
            cleanup_interval_secs = self.config.cleanup_interval.as_secs(),
            session_timeout_secs = self.config.session_timeout.as_secs(),
            "Server transport loops spawned"
        );
        Ok(())
    }

    /// Close the transport.
    pub async fn close(&mut self) -> Result<()> {
        self.base.disconnect().await?;
        self.sessions.write().await.clear();
        self.event_to_client.write().await.clear();
        Ok(())
    }

    /// Send a response back to the client that sent the original request.
    pub async fn send_response(&self, event_id: &str, mut response: JsonRpcMessage) -> Result<()> {
        let event_to_client = self.event_to_client.read().await;
        let client_pubkey_hex = event_to_client
            .get(event_id)
            .ok_or_else(|| {
                tracing::error!(
                    target: LOG_TARGET,
                    event_id = %event_id,
                    "No client found for response correlation"
                );
                Error::Other(format!("No client found for event {event_id}"))
            })?
            .clone();
        drop(event_to_client);

        let sessions = self.sessions.read().await;
        let session = sessions.get(&client_pubkey_hex).ok_or_else(|| {
            tracing::error!(
                target: LOG_TARGET,
                client_pubkey = %client_pubkey_hex,
                "No session for correlated client"
            );
            Error::Other(format!("No session for client {client_pubkey_hex}"))
        })?;

        // Restore original request ID
        if let Some(original_id) = session.pending_requests.get(event_id) {
            match &mut response {
                JsonRpcMessage::Response(r) => r.id = original_id.clone(),
                JsonRpcMessage::ErrorResponse(r) => r.id = original_id.clone(),
                _ => {}
            }
        }

        let is_encrypted = session.is_encrypted;
        drop(sessions);

        let client_pubkey = PublicKey::from_hex(&client_pubkey_hex).map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                client_pubkey = %client_pubkey_hex,
                "Invalid client pubkey in session map"
            );
            Error::Other(error.to_string())
        })?;

        let event_id_parsed = EventId::from_hex(event_id).map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                event_id = %event_id,
                "Invalid event id while sending response"
            );
            Error::Other(error.to_string())
        })?;

        let tags = BaseTransport::create_response_tags(&client_pubkey, &event_id_parsed);

        self.base
            .send_mcp_message(
                &response,
                &client_pubkey,
                CTXVM_MESSAGES_KIND,
                tags,
                Some(is_encrypted),
            )
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    client_pubkey = %client_pubkey_hex,
                    event_id = %event_id,
                    "Failed to publish response message"
                );
                error
            })?;

        // Clean up
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&client_pubkey_hex) {
            // Clean up progress token
            if let Some(token) = session.event_to_progress_token.remove(event_id) {
                session.pending_requests.remove(&token);
            }
            session.pending_requests.remove(event_id);
        }
        drop(sessions);

        self.event_to_client.write().await.remove(event_id);

        tracing::debug!(
            target: LOG_TARGET,
            client_pubkey = %client_pubkey_hex,
            event_id = %event_id,
            encrypted = is_encrypted,
            "Sent server response and cleaned correlation state"
        );
        Ok(())
    }

    /// Send a notification to a specific client.
    pub async fn send_notification(
        &self,
        client_pubkey_hex: &str,
        notification: &JsonRpcMessage,
        correlated_event_id: Option<&str>,
    ) -> Result<()> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(client_pubkey_hex)
            .ok_or_else(|| Error::Other(format!("No session for {client_pubkey_hex}")))?;
        let is_encrypted = session.is_encrypted;
        drop(sessions);

        let client_pubkey =
            PublicKey::from_hex(client_pubkey_hex).map_err(|e| Error::Other(e.to_string()))?;

        let mut tags = BaseTransport::create_recipient_tags(&client_pubkey);
        if let Some(eid) = correlated_event_id {
            let event_id = EventId::from_hex(eid).map_err(|e| Error::Other(e.to_string()))?;
            tags.push(Tag::event(event_id));
        }

        self.base
            .send_mcp_message(
                notification,
                &client_pubkey,
                CTXVM_MESSAGES_KIND,
                tags,
                Some(is_encrypted),
            )
            .await?;

        Ok(())
    }

    /// Broadcast a notification to all initialized clients.
    pub async fn broadcast_notification(&self, notification: &JsonRpcMessage) -> Result<()> {
        let sessions = self.sessions.read().await;
        let initialized: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.is_initialized)
            .map(|(k, _)| k.clone())
            .collect();
        drop(sessions);

        for pubkey in initialized {
            if let Err(error) = self.send_notification(&pubkey, notification, None).await {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    client_pubkey = %pubkey,
                    "Failed to send notification"
                );
            }
        }
        Ok(())
    }

    /// Take the message receiver for consuming incoming requests.
    pub fn take_message_receiver(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>> {
        self.message_rx.take()
    }

    /// Publish server announcement (kind 11316).
    pub async fn announce(&self) -> Result<EventId> {
        let info = self
            .config
            .server_info
            .as_ref()
            .ok_or_else(|| Error::Other("No server info configured".to_string()))?;

        let content = serde_json::to_string(info)?;

        let mut tags = Vec::new();
        if let Some(ref name) = info.name {
            tags.push(Tag::custom(
                TagKind::Custom(tags::NAME.into()),
                vec![name.clone()],
            ));
        }
        if let Some(ref about) = info.about {
            tags.push(Tag::custom(
                TagKind::Custom(tags::ABOUT.into()),
                vec![about.clone()],
            ));
        }
        if let Some(ref website) = info.website {
            tags.push(Tag::custom(
                TagKind::Custom(tags::WEBSITE.into()),
                vec![website.clone()],
            ));
        }
        if let Some(ref picture) = info.picture {
            tags.push(Tag::custom(
                TagKind::Custom(tags::PICTURE.into()),
                vec![picture.clone()],
            ));
        }
        if self.config.encryption_mode != EncryptionMode::Disabled {
            tags.push(Tag::custom(
                TagKind::Custom(tags::SUPPORT_ENCRYPTION.into()),
                Vec::<String>::new(),
            ));
            tags.push(Tag::custom(
                TagKind::Custom(tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
                Vec::<String>::new(),
            ));
        }

        let builder = EventBuilder::new(Kind::Custom(SERVER_ANNOUNCEMENT_KIND), content).tags(tags);

        self.base.relay_pool.publish(builder).await
    }

    /// Publish tools list (kind 11317).
    pub async fn publish_tools(&self, tools: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "tools": tools });
        let builder = EventBuilder::new(
            Kind::Custom(TOOLS_LIST_KIND),
            serde_json::to_string(&content)?,
        );
        self.base.relay_pool.publish(builder).await
    }

    /// Publish resources list (kind 11318).
    pub async fn publish_resources(&self, resources: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "resources": resources });
        let builder = EventBuilder::new(
            Kind::Custom(RESOURCES_LIST_KIND),
            serde_json::to_string(&content)?,
        );
        self.base.relay_pool.publish(builder).await
    }

    /// Publish prompts list (kind 11320).
    pub async fn publish_prompts(&self, prompts: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "prompts": prompts });
        let builder = EventBuilder::new(
            Kind::Custom(PROMPTS_LIST_KIND),
            serde_json::to_string(&content)?,
        );
        self.base.relay_pool.publish(builder).await
    }

    /// Publish resource templates list (kind 11319).
    pub async fn publish_resource_templates(
        &self,
        templates: Vec<serde_json::Value>,
    ) -> Result<EventId> {
        let content = serde_json::json!({ "resourceTemplates": templates });
        let builder = EventBuilder::new(
            Kind::Custom(RESOURCETEMPLATES_LIST_KIND),
            serde_json::to_string(&content)?,
        );
        self.base.relay_pool.publish(builder).await
    }

    /// Delete server announcements (NIP-09 kind 5).
    pub async fn delete_announcements(&self, reason: &str) -> Result<()> {
        // We publish kind 5 events for each announcement kind
        let pubkey = self.base.get_public_key().await?;
        let _pubkey_hex = pubkey.to_hex();

        for kind in UNENCRYPTED_KINDS {
            let builder = EventBuilder::new(Kind::Custom(5), reason).tag(Tag::custom(
                TagKind::Custom("k".into()),
                vec![kind.to_string()],
            ));
            self.base.relay_pool.publish(builder).await?;
        }
        Ok(())
    }

    /// Publish tools list from rmcp typed tool descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_tools_typed(&self, tools: Vec<rmcp::model::Tool>) -> Result<EventId> {
        let tools = tools
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_tools(tools).await
    }

    /// Publish resources list from rmcp typed resource descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_resources_typed(
        &self,
        resources: Vec<rmcp::model::Resource>,
    ) -> Result<EventId> {
        let resources = resources
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_resources(resources).await
    }

    /// Publish prompts list from rmcp typed prompt descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_prompts_typed(
        &self,
        prompts: Vec<rmcp::model::Prompt>,
    ) -> Result<EventId> {
        let prompts = prompts
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_prompts(prompts).await
    }

    /// Publish resource templates list from rmcp typed template descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_resource_templates_typed(
        &self,
        templates: Vec<rmcp::model::ResourceTemplate>,
    ) -> Result<EventId> {
        let templates = templates
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_resource_templates(templates).await
    }

    // ── Internal ────────────────────────────────────────────────

    fn is_capability_excluded(
        excluded: &[CapabilityExclusion],
        method: &str,
        name: Option<&str>,
    ) -> bool {
        // Always allow fundamental MCP methods
        if method == "initialize" || method == "notifications/initialized" {
            return true;
        }

        excluded.iter().any(|excl| {
            if excl.method != method {
                return false;
            }
            match (&excl.name, name) {
                (Some(excl_name), Some(req_name)) => excl_name == req_name,
                (None, _) => true, // method-only match
                _ => false,
            }
        })
    }

    async fn event_loop(
        client: Arc<Client>,
        sessions: Arc<RwLock<HashMap<String, ClientSession>>>,
        event_to_client: Arc<RwLock<HashMap<String, String>>>,
        tx: tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
        allowed_pubkeys: Vec<String>,
        excluded_capabilities: Vec<CapabilityExclusion>,
        encryption_mode: EncryptionMode,
    ) {
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                let (content, sender_pubkey, event_id, is_encrypted) = if event.kind
                    == Kind::Custom(GIFT_WRAP_KIND)
                    || event.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)
                {
                    if encryption_mode == EncryptionMode::Disabled {
                        tracing::warn!(
                            target: LOG_TARGET,
                            event_id = %event.id.to_hex(),
                            sender_pubkey = %event.pubkey.to_hex(),
                            "Received encrypted message but encryption is disabled"
                        );
                        continue;
                    }
                    // Single-layer NIP-44 decrypt (matches JS/TS SDK)
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
                            // The decrypted content is JSON of the inner signed event.
                            // Use the INNER event's ID for correlation — the client
                            // registers the inner event ID in its correlation store.
                            match serde_json::from_str::<Event>(&decrypted_json) {
                                Ok(inner) => {
                                    if let Err(e) = inner.verify() {
                                        tracing::warn!(
                                            "Inner event signature verification failed: {e}"
                                        );
                                        continue;
                                    }
                                    (
                                        inner.content,
                                        inner.pubkey.to_hex(),
                                        inner.id.to_hex(),
                                        true,
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
                                "Failed to decrypt"
                            );
                            continue;
                        }
                    }
                } else {
                    if encryption_mode == EncryptionMode::Required {
                        tracing::warn!(
                            target: LOG_TARGET,
                            sender_pubkey = %event.pubkey.to_hex(),
                            "Received unencrypted message but encryption is required"
                        );
                        continue;
                    }
                    (
                        event.content.clone(),
                        event.pubkey.to_hex(),
                        event.id.to_hex(),
                        false,
                    )
                };

                // Parse MCP message
                let mcp_msg = match validation::validate_and_parse(&content) {
                    Some(msg) => msg,
                    None => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            sender_pubkey = %sender_pubkey,
                            "Invalid MCP message"
                        );
                        continue;
                    }
                };

                // Authorization check
                if !allowed_pubkeys.is_empty() {
                    let method = mcp_msg.method().unwrap_or("");
                    let name = match &mcp_msg {
                        JsonRpcMessage::Request(r) => r
                            .params
                            .as_ref()
                            .and_then(|p| p.get("name"))
                            .and_then(|n| n.as_str()),
                        _ => None,
                    };

                    let is_excluded =
                        Self::is_capability_excluded(&excluded_capabilities, method, name);

                    if !allowed_pubkeys.contains(&sender_pubkey) && !is_excluded {
                        tracing::warn!(
                            target: LOG_TARGET,
                            sender_pubkey = %sender_pubkey,
                            method = method,
                            "Unauthorized request"
                        );
                        continue;
                    }
                }

                // Session management
                let mut sessions_w = sessions.write().await;
                let session = sessions_w
                    .entry(sender_pubkey.clone())
                    .or_insert_with(|| ClientSession::new(is_encrypted));
                session.update_activity();
                session.is_encrypted = is_encrypted;

                // Track request for correlation
                if let JsonRpcMessage::Request(ref req) = mcp_msg {
                    let original_id = req.id.clone();
                    session
                        .pending_requests
                        .insert(event_id.clone(), original_id);
                    event_to_client
                        .write()
                        .await
                        .insert(event_id.clone(), sender_pubkey.clone());

                    // Track progress token
                    if let Some(token) = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                        .and_then(|t| t.as_str())
                    {
                        session
                            .pending_requests
                            .insert(token.to_string(), serde_json::json!(event_id));
                        session
                            .event_to_progress_token
                            .insert(event_id.clone(), token.to_string());
                    }
                }

                // Handle initialized notification
                if let JsonRpcMessage::Notification(ref n) = mcp_msg {
                    if n.method == "notifications/initialized" {
                        session.is_initialized = true;
                    }
                }

                drop(sessions_w);

                // Forward to consumer
                let _ = tx.send(IncomingRequest {
                    message: mcp_msg,
                    client_pubkey: sender_pubkey,
                    event_id,
                    is_encrypted,
                });
            }
        }
    }

    async fn cleanup_sessions(
        sessions: &RwLock<HashMap<String, ClientSession>>,
        event_to_client: &RwLock<HashMap<String, String>>,
        timeout: Duration,
    ) -> usize {
        let mut sessions_w = sessions.write().await;
        let mut event_map = event_to_client.write().await;
        let mut cleaned = 0;

        sessions_w.retain(|pubkey, session| {
            if session.last_activity.elapsed() > timeout {
                // Clean up reverse mappings
                for event_id in session.pending_requests.keys() {
                    event_map.remove(event_id);
                }
                for event_id in session.event_to_progress_token.keys() {
                    event_map.remove(event_id);
                }
                tracing::debug!(
                    target: LOG_TARGET,
                    client_pubkey = %pubkey,
                    "Session expired"
                );
                cleaned += 1;
                false
            } else {
                true
            }
        });

        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // ── Session management ──────────────────────────────────────

    #[test]
    fn test_client_session_creation() {
        let session = ClientSession::new(true);
        assert!(!session.is_initialized);
        assert!(session.is_encrypted);
        assert!(session.pending_requests.is_empty());
        assert!(session.event_to_progress_token.is_empty());
    }

    #[test]
    fn test_client_session_update_activity() {
        let mut session = ClientSession::new(false);
        let first = session.last_activity;
        thread::sleep(Duration::from_millis(10));
        session.update_activity();
        assert!(session.last_activity > first);
    }

    #[tokio::test]
    async fn test_cleanup_sessions_removes_expired() {
        let sessions = Arc::new(RwLock::new(HashMap::new()));
        let event_to_client = Arc::new(RwLock::new(HashMap::new()));

        // Insert a session with an old activity time
        let mut session = ClientSession::new(false);
        session
            .pending_requests
            .insert("evt1".to_string(), serde_json::json!(1));
        sessions
            .write()
            .await
            .insert("pubkey1".to_string(), session);
        event_to_client
            .write()
            .await
            .insert("evt1".to_string(), "pubkey1".to_string());

        // With a long timeout, nothing should be cleaned
        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_to_client,
            Duration::from_secs(300),
        )
        .await;
        assert_eq!(cleaned, 0);
        assert_eq!(sessions.read().await.len(), 1);

        // With zero timeout, it should be cleaned
        thread::sleep(Duration::from_millis(5));
        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_to_client,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(cleaned, 1);
        assert!(sessions.read().await.is_empty());
        assert!(event_to_client.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_cleanup_preserves_active_sessions() {
        let sessions = Arc::new(RwLock::new(HashMap::new()));
        let event_to_client = Arc::new(RwLock::new(HashMap::new()));

        let session = ClientSession::new(false);
        sessions.write().await.insert("active".to_string(), session);

        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_to_client,
            Duration::from_secs(300),
        )
        .await;
        assert_eq!(cleaned, 0);
        assert_eq!(sessions.read().await.len(), 1);
    }

    // ── Request ID correlation ──────────────────────────────────

    #[test]
    fn test_pending_request_tracking() {
        let mut session = ClientSession::new(false);
        session
            .pending_requests
            .insert("event_abc".to_string(), serde_json::json!(42));
        assert_eq!(
            session.pending_requests.get("event_abc"),
            Some(&serde_json::json!(42))
        );
    }

    #[test]
    fn test_progress_token_tracking() {
        let mut session = ClientSession::new(false);
        session
            .event_to_progress_token
            .insert("evt1".to_string(), "token1".to_string());
        session
            .pending_requests
            .insert("token1".to_string(), serde_json::json!("evt1"));
        assert_eq!(
            session.event_to_progress_token.get("evt1"),
            Some(&"token1".to_string())
        );
    }

    // ── Authorization (is_capability_excluded) ──────────────────

    #[test]
    fn test_initialize_always_excluded() {
        assert!(NostrServerTransport::is_capability_excluded(
            &[],
            "initialize",
            None
        ));
        assert!(NostrServerTransport::is_capability_excluded(
            &[],
            "notifications/initialized",
            None
        ));
    }

    #[test]
    fn test_method_excluded_without_name() {
        let exclusions = vec![CapabilityExclusion {
            method: "tools/list".to_string(),
            name: None,
        }];
        assert!(NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/list",
            None
        ));
        assert!(NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/list",
            Some("anything")
        ));
    }

    #[test]
    fn test_method_excluded_with_name() {
        let exclusions = vec![CapabilityExclusion {
            method: "tools/call".to_string(),
            name: Some("get_weather".to_string()),
        }];
        assert!(NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/call",
            Some("get_weather")
        ));
        assert!(!NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/call",
            Some("other_tool")
        ));
        assert!(!NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/call",
            None
        ));
    }

    #[test]
    fn test_non_excluded_method() {
        let exclusions = vec![CapabilityExclusion {
            method: "tools/list".to_string(),
            name: None,
        }];
        assert!(!NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/call",
            None
        ));
        assert!(!NostrServerTransport::is_capability_excluded(
            &exclusions,
            "resources/list",
            None
        ));
    }

    #[test]
    fn test_empty_exclusions_non_init_method() {
        assert!(!NostrServerTransport::is_capability_excluded(
            &[],
            "tools/list",
            None
        ));
        assert!(!NostrServerTransport::is_capability_excluded(
            &[],
            "tools/call",
            Some("x")
        ));
    }

    // ── Encryption mode enforcement ─────────────────────────────

    #[test]
    fn test_encryption_mode_default() {
        let config = NostrServerTransportConfig::default();
        assert_eq!(config.encryption_mode, EncryptionMode::Optional);
    }

    // ── Config defaults ─────────────────────────────────────────

    #[test]
    fn test_config_defaults() {
        let config = NostrServerTransportConfig::default();
        assert_eq!(config.relay_urls, vec!["wss://relay.damus.io".to_string()]);
        assert!(!config.is_announced_server);
        assert!(config.allowed_public_keys.is_empty());
        assert!(config.excluded_capabilities.is_empty());
        assert_eq!(config.cleanup_interval, Duration::from_secs(60));
        assert_eq!(config.session_timeout, Duration::from_secs(300));
        assert!(config.server_info.is_none());
        assert!(config.log_file_path.is_none());
    }
}
