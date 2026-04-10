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
    /// Gift-wrap policy for encrypted messages.
    pub gift_wrap_mode: GiftWrapMode,
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
            gift_wrap_mode: GiftWrapMode::Optional,
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
    /// Extra common discovery tags to include in server announcements and first responses.
    extra_common_tags: Vec<Tag>,
    /// Pricing tags to include in announcements and capability list responses.
    pricing_tags: Vec<Tag>,
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
            gift_wrap_mode = ?config.gift_wrap_mode,
            "Created server transport"
        );
        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            config,
            extra_common_tags: Vec::new(),
            pricing_tags: Vec::new(),
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
        let relay_pool = self.base.relay_pool.clone();
        let sessions = self.sessions.clone();
        let event_to_client = self.event_to_client.clone();
        let tx = self.message_tx.clone();
        let allowed = self.config.allowed_public_keys.clone();
        let excluded = self.config.excluded_capabilities.clone();
        let encryption_mode = self.config.encryption_mode;
        let gift_wrap_mode = self.config.gift_wrap_mode;

        tokio::spawn(async move {
            Self::event_loop(
                relay_pool,
                sessions,
                event_to_client,
                tx,
                allowed,
                excluded,
                encryption_mode,
                gift_wrap_mode,
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

        let route = session
            .pending_requests
            .get(event_id)
            .cloned()
            .ok_or_else(|| {
                tracing::error!(
                    target: LOG_TARGET,
                    client_pubkey = %client_pubkey_hex,
                    event_id = %event_id,
                    "No pending route for correlated response"
                );
                Error::Other(format!("No pending route for event {event_id}"))
            })?;
        let is_encrypted = session.is_encrypted;
        let should_attach_common_tags = !session.has_sent_common_tags;
        drop(sessions);

        // Restore original request ID
        match &mut response {
            JsonRpcMessage::Response(r) => r.id = route.original_request_id.clone(),
            JsonRpcMessage::ErrorResponse(r) => r.id = route.original_request_id.clone(),
            _ => {}
        }

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

        let mut tags = BaseTransport::create_response_tags(&client_pubkey, &event_id_parsed);
        if should_attach_common_tags {
            Self::append_common_response_tags(
                &mut tags,
                self.config.server_info.as_ref(),
                &self.extra_common_tags,
                self.config.encryption_mode,
                self.config.gift_wrap_mode,
            );
        } else if Self::is_initialize_result_response(&response) {
            tags.extend(Self::server_info_tags(self.config.server_info.as_ref()));
        }
        if Self::is_capability_list_response(&response) {
            tags.extend(self.pricing_tags.iter().cloned());
        }

        self.base
            .send_mcp_message(
                &response,
                &client_pubkey,
                CTXVM_MESSAGES_KIND,
                tags,
                Some(is_encrypted),
                Self::select_outbound_gift_wrap_kind(
                    self.config.gift_wrap_mode,
                    is_encrypted,
                    route.wrap_kind,
                ),
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
            if let Some(removed_route) = session.pending_requests.remove(event_id) {
                if let Some(token) = removed_route.progress_token {
                    session.progress_token_to_event.remove(&token);
                }
            }
            if should_attach_common_tags {
                session.has_sent_common_tags = true;
            }
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
        let progress_token = Self::progress_token_from_notification(notification);
        let correlated_event_id =
            Self::resolve_notification_correlation(session, notification, correlated_event_id);
        if progress_token.is_some() && correlated_event_id.is_none() {
            return Err(Error::Other(format!(
                "No request route found for progress token in client session {client_pubkey_hex}"
            )));
        }
        drop(sessions);

        let client_pubkey =
            PublicKey::from_hex(client_pubkey_hex).map_err(|e| Error::Other(e.to_string()))?;

        let mut tags = BaseTransport::create_recipient_tags(&client_pubkey);
        if let Some(ref eid) = correlated_event_id {
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
                Self::select_outbound_notification_gift_wrap_kind(
                    self.config.gift_wrap_mode,
                    is_encrypted,
                ),
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

    /// Sets extra discovery tags to include in announcements and first-response discovery replay.
    pub fn set_announcement_extra_tags(&mut self, tags: Vec<Tag>) {
        self.extra_common_tags = tags;
    }

    /// Sets pricing tags to include in announcement/list events and capability list responses.
    pub fn set_announcement_pricing_tags(&mut self, tags: Vec<Tag>) {
        self.pricing_tags = tags;
    }

    /// Publish server announcement (kind 11316).
    pub async fn announce(&self) -> Result<EventId> {
        let info = self
            .config
            .server_info
            .as_ref()
            .ok_or_else(|| Error::Other("No server info configured".to_string()))?;

        let content = serde_json::to_string(info)?;

        let mut tags = Self::server_info_tags(Some(info));
        Self::append_transport_capability_tags(
            &mut tags,
            self.config.encryption_mode,
            self.config.gift_wrap_mode,
        );
        tags.extend(self.extra_common_tags.iter().cloned());
        tags.extend(self.pricing_tags.iter().cloned());

        let builder = EventBuilder::new(Kind::Custom(SERVER_ANNOUNCEMENT_KIND), content).tags(tags);

        self.base.relay_pool.publish(builder).await
    }

    /// Publish tools list (kind 11317).
    pub async fn publish_tools(&self, tools: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "tools": tools });
        let builder = EventBuilder::new(
            Kind::Custom(TOOLS_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.base.relay_pool.publish(builder).await
    }

    /// Publish resources list (kind 11318).
    pub async fn publish_resources(&self, resources: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "resources": resources });
        let builder = EventBuilder::new(
            Kind::Custom(RESOURCES_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.base.relay_pool.publish(builder).await
    }

    /// Publish prompts list (kind 11320).
    pub async fn publish_prompts(&self, prompts: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "prompts": prompts });
        let builder = EventBuilder::new(
            Kind::Custom(PROMPTS_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
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
        )
        .tags(self.pricing_tags.iter().cloned());
        self.base.relay_pool.publish(builder).await
    }

    /// Delete server announcements (NIP-09 kind 5).
    pub async fn delete_announcements(&self, reason: &str) -> Result<()> {
        let pubkey = self.base.get_public_key().await?;
        for kind in UNENCRYPTED_KINDS {
            let filter = Filter::new().kind(Kind::Custom(*kind)).author(pubkey);
            let events = self
                .base
                .relay_pool
                .client()
                .fetch_events(filter, Duration::from_secs(10))
                .await
                .map_err(|error| Error::Transport(error.to_string()))?;
            let deletion_tags: Vec<Tag> = events
                .into_iter()
                .map(|event| Tag::event(event.id))
                .collect();
            if deletion_tags.is_empty() {
                continue;
            }
            let builder = EventBuilder::new(Kind::Custom(5), reason).tags(deletion_tags);
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

    fn server_info_tags(server_info: Option<&ServerInfo>) -> Vec<Tag> {
        let mut tags = Vec::new();
        let Some(info) = server_info else {
            return tags;
        };

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

        tags
    }

    fn append_transport_capability_tags(
        tags: &mut Vec<Tag>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
    ) {
        if encryption_mode == EncryptionMode::Disabled {
            return;
        }

        tags.push(Tag::custom(
            TagKind::Custom(crate::core::constants::tags::SUPPORT_ENCRYPTION.into()),
            Vec::<String>::new(),
        ));

        if gift_wrap_mode.supports_ephemeral() {
            tags.push(Tag::custom(
                TagKind::Custom(crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
                Vec::<String>::new(),
            ));
        }
    }

    fn append_common_response_tags(
        tags: &mut Vec<Tag>,
        server_info: Option<&ServerInfo>,
        extra_common_tags: &[Tag],
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
    ) {
        tags.extend(Self::server_info_tags(server_info));
        Self::append_transport_capability_tags(tags, encryption_mode, gift_wrap_mode);
        tags.extend(extra_common_tags.iter().cloned());
    }

    fn select_outbound_gift_wrap_kind(
        gift_wrap_mode: GiftWrapMode,
        is_encrypted: bool,
        mirrored_kind: Option<u16>,
    ) -> Option<u16> {
        if !is_encrypted {
            return None;
        }

        Some(match gift_wrap_mode {
            GiftWrapMode::Persistent => GIFT_WRAP_KIND,
            GiftWrapMode::Ephemeral => EPHEMERAL_GIFT_WRAP_KIND,
            GiftWrapMode::Optional => match mirrored_kind {
                Some(kind) if kind == EPHEMERAL_GIFT_WRAP_KIND => EPHEMERAL_GIFT_WRAP_KIND,
                _ => GIFT_WRAP_KIND,
            },
        })
    }

    fn select_outbound_notification_gift_wrap_kind(
        gift_wrap_mode: GiftWrapMode,
        is_encrypted: bool,
    ) -> Option<u16> {
        if !is_encrypted {
            return None;
        }

        match gift_wrap_mode {
            GiftWrapMode::Ephemeral => Some(EPHEMERAL_GIFT_WRAP_KIND),
            GiftWrapMode::Persistent => Some(GIFT_WRAP_KIND),
            GiftWrapMode::Optional => None,
        }
    }

    fn is_initialize_result_response(message: &JsonRpcMessage) -> bool {
        let JsonRpcMessage::Response(response) = message else {
            return false;
        };
        response.result.get("protocolVersion").is_some()
            && response.result.get("serverInfo").is_some()
    }

    fn is_capability_list_response(message: &JsonRpcMessage) -> bool {
        let JsonRpcMessage::Response(response) = message else {
            return false;
        };
        let result = &response.result;
        result.get("tools").is_some()
            || result.get("resources").is_some()
            || result.get("resourceTemplates").is_some()
            || result.get("prompts").is_some()
    }

    fn progress_token_from_notification(notification: &JsonRpcMessage) -> Option<String> {
        let JsonRpcMessage::Notification(notification) = notification else {
            return None;
        };
        if notification.method != "notifications/progress" {
            return None;
        }
        notification
            .params
            .as_ref()
            .and_then(|params| params.get("progressToken"))
            .and_then(|token| token.as_str())
            .map(str::to_string)
    }

    fn resolve_notification_correlation(
        session: &ClientSession,
        notification: &JsonRpcMessage,
        correlated_event_id: Option<&str>,
    ) -> Option<String> {
        if let Some(event_id) = correlated_event_id {
            return Some(event_id.to_string());
        }

        let progress_token = Self::progress_token_from_notification(notification)?;
        session
            .progress_token_to_event
            .get(&progress_token)
            .cloned()
    }

    fn unauthorized_error_response(request_id: &serde_json::Value) -> JsonRpcMessage {
        JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: request_id.clone(),
            error: JsonRpcError {
                code: -32000,
                message: "Unauthorized".to_string(),
                data: None,
            },
        })
    }

    async fn event_loop(
        relay_pool: Arc<RelayPool>,
        sessions: Arc<RwLock<HashMap<String, ClientSession>>>,
        event_to_client: Arc<RwLock<HashMap<String, String>>>,
        tx: tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
        allowed_pubkeys: Vec<String>,
        excluded_capabilities: Vec<CapabilityExclusion>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
    ) {
        let client = relay_pool.client().clone();
        let transport = BaseTransport {
            relay_pool,
            encryption_mode,
            is_connected: true,
        };
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                let outer_kind = event.kind.as_u16();
                let (content, sender_pubkey, event_id, is_encrypted, incoming_gift_wrap_kind) =
                    if outer_kind == GIFT_WRAP_KIND || outer_kind == EPHEMERAL_GIFT_WRAP_KIND
                    {
                        let event_kind = outer_kind;
                        if !gift_wrap_mode.allows_kind(event_kind) {
                            tracing::warn!(
                                target: LOG_TARGET,
                                event_id = %event.id.to_hex(),
                                event_kind = event_kind,
                                configured_mode = ?gift_wrap_mode,
                                "Skipping gift wrap due to CEP-19 policy"
                            );
                            continue;
                        }
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
                            None,
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
                        if let JsonRpcMessage::Request(ref request) = mcp_msg {
                            if let Ok(client_pubkey) = PublicKey::from_hex(&sender_pubkey) {
                                let mut tags = BaseTransport::create_response_tags(
                                    &client_pubkey,
                                    &EventId::from_hex(&event_id).unwrap_or(event.id),
                                );
                                if let Err(error) = transport
                                    .send_mcp_message(
                                        &Self::unauthorized_error_response(&request.id),
                                        &client_pubkey,
                                        CTXVM_MESSAGES_KIND,
                                        std::mem::take(&mut tags),
                                        Some(is_encrypted),
                                        Self::select_outbound_gift_wrap_kind(
                                            gift_wrap_mode,
                                            is_encrypted,
                                            incoming_gift_wrap_kind,
                                        ),
                                    )
                                    .await
                                {
                                    tracing::error!(
                                        target: LOG_TARGET,
                                        error = %error,
                                        client_pubkey = %sender_pubkey,
                                        "Failed to send unauthorized response"
                                    );
                                }
                            }
                        }
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
                    let progress_token = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                        .and_then(|t| t.as_str())
                        .map(str::to_string);
                    session.pending_requests.insert(
                        event_id.clone(),
                        PendingRequestRoute {
                            original_request_id: req.id.clone(),
                            progress_token: progress_token.clone(),
                            wrap_kind: incoming_gift_wrap_kind,
                        },
                    );
                    event_to_client
                        .write()
                        .await
                        .insert(event_id.clone(), sender_pubkey.clone());

                    if let Some(token) = progress_token {
                        session
                            .progress_token_to_event
                            .insert(token, event_id.clone());
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
        assert!(!session.has_sent_common_tags);
        assert!(session.is_encrypted);
        assert!(session.pending_requests.is_empty());
        assert!(session.progress_token_to_event.is_empty());
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
        session.pending_requests.insert(
            "evt1".to_string(),
            PendingRequestRoute {
                original_request_id: serde_json::json!(1),
                progress_token: Some("token1".to_string()),
                wrap_kind: None,
            },
        );
        session
            .progress_token_to_event
            .insert("token1".to_string(), "evt1".to_string());
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
        let route = PendingRequestRoute {
            original_request_id: serde_json::json!(42),
            progress_token: None,
            wrap_kind: Some(GIFT_WRAP_KIND),
        };
        session
            .pending_requests
            .insert("event_abc".to_string(), route.clone());
        assert_eq!(
            session
                .pending_requests
                .get("event_abc")
                .map(|route| route.original_request_id.clone()),
            Some(serde_json::json!(42))
        );
        assert_eq!(
            session
                .pending_requests
                .get("event_abc")
                .and_then(|route| route.wrap_kind),
            route.wrap_kind
        );
    }

    #[test]
    fn test_progress_token_tracking() {
        let mut session = ClientSession::new(false);
        session
            .progress_token_to_event
            .insert("token1".to_string(), "evt1".to_string());
        session.pending_requests.insert(
            "evt1".to_string(),
            PendingRequestRoute {
                original_request_id: serde_json::json!("evt1"),
                progress_token: Some("token1".to_string()),
                wrap_kind: None,
            },
        );
        assert_eq!(
            session.progress_token_to_event.get("token1"),
            Some(&"evt1".to_string())
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
        assert_eq!(config.gift_wrap_mode, GiftWrapMode::Optional);
    }

    #[test]
    fn test_append_transport_capability_tags_respects_gift_wrap_mode() {
        let mut tags = Vec::new();
        NostrServerTransport::append_transport_capability_tags(
            &mut tags,
            EncryptionMode::Optional,
            GiftWrapMode::Persistent,
        );
        let rendered: Vec<Vec<String>> = tags.iter().cloned().map(|t| t.to_vec()).collect();
        assert!(rendered
            .iter()
            .any(|t| t[0] == crate::core::constants::tags::SUPPORT_ENCRYPTION));
        assert!(!rendered
            .iter()
            .any(|t| { t[0] == crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL }));

        let mut tags = Vec::new();
        NostrServerTransport::append_transport_capability_tags(
            &mut tags,
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
        );
        let rendered: Vec<Vec<String>> = tags.iter().cloned().map(|t| t.to_vec()).collect();
        assert!(rendered
            .iter()
            .any(|t| { t[0] == crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL }));
    }

    #[test]
    fn test_select_outbound_gift_wrap_kind_optional_mode_mirrors_client_kind() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                Some(EPHEMERAL_GIFT_WRAP_KIND),
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                Some(GIFT_WRAP_KIND),
            ),
            Some(GIFT_WRAP_KIND)
        );
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Persistent,
                true,
                Some(EPHEMERAL_GIFT_WRAP_KIND),
            ),
            Some(GIFT_WRAP_KIND)
        );
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Ephemeral,
                true,
                Some(GIFT_WRAP_KIND),
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_notification_gift_wrap_kind_optional_mode_uses_default_persistent() {
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
            ),
            None
        );
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Persistent,
                true,
            ),
            Some(GIFT_WRAP_KIND)
        );
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Ephemeral,
                true,
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_resolve_notification_correlation_uses_progress_token_mapping() {
        let mut session = ClientSession::new(true);
        session
            .progress_token_to_event
            .insert("token1".to_string(), "evt1".to_string());
        let notification = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(serde_json::json!({ "progressToken": "token1" })),
        });

        assert_eq!(
            NostrServerTransport::resolve_notification_correlation(&session, &notification, None),
            Some("evt1".to_string())
        );
    }

    #[test]
    fn test_common_response_tags_include_server_info_and_transport_capabilities() {
        let mut tags = Vec::new();
        NostrServerTransport::append_common_response_tags(
            &mut tags,
            Some(&ServerInfo {
                name: Some("Demo".to_string()),
                ..Default::default()
            }),
            &[Tag::custom(
                TagKind::Custom("x-demo".into()),
                Vec::<String>::new(),
            )],
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
        );
        let rendered: Vec<Vec<String>> = tags.iter().cloned().map(|tag| tag.to_vec()).collect();
        assert!(rendered
            .iter()
            .any(|tag| tag[0] == crate::core::constants::tags::NAME));
        assert!(rendered
            .iter()
            .any(|tag| tag[0] == crate::core::constants::tags::SUPPORT_ENCRYPTION));
        assert!(rendered.iter().any(|tag| tag[0] == "x-demo"));
    }

    #[test]
    fn test_unauthorized_error_response_shape() {
        let response = NostrServerTransport::unauthorized_error_response(&serde_json::json!(1));
        let JsonRpcMessage::ErrorResponse(response) = response else {
            panic!("expected JSON-RPC error response");
        };
        assert_eq!(response.id, serde_json::json!(1));
        assert_eq!(response.error.code, -32000);
        assert_eq!(response.error.message, "Unauthorized");
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
        assert_eq!(config.gift_wrap_mode, GiftWrapMode::Optional);
        assert!(config.server_info.is_none());
        assert!(config.log_file_path.is_none());
    }
}
