//! Server-side Nostr transport for ContextVM.
//!
//! Listens for incoming MCP requests from clients over Nostr, manages multi-client
//! sessions, handles request/response correlation, and optionally publishes
//! server announcements.

pub(crate) mod announcement_manager;
pub mod correlation_store;
pub mod session_store;

pub use correlation_store::{RouteEntry, ServerEventRouteStore};
pub use session_store::{SessionSnapshot, SessionStore};
use tokio::sync::RwLock;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lru::LruCache;
use nostr_sdk::prelude::*;
use tokio_util::sync::CancellationToken;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::types::*;
use crate::core::validation;
use crate::encryption;
use crate::relay::{RelayPool, RelayPoolTrait};
use crate::transport::base::BaseTransport;
use crate::transport::discovery_tags::learn_peer_capabilities;

const LOG_TARGET: &str = "contextvm_sdk::transport::server";

/// Configuration for the server transport.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NostrServerTransportConfig {
    /// Relay URLs to connect to.
    pub relay_urls: Vec<String>,
    /// Encryption mode.
    pub encryption_mode: EncryptionMode,
    /// Gift-wrap kind selection policy (CEP-19).
    pub gift_wrap_mode: GiftWrapMode,
    /// Server information for announcements.
    pub server_info: Option<ServerInfo>,
    /// Whether this server publishes public announcements (CEP-6).
    pub is_announced_server: bool,
    /// Allowed client public keys (hex). Empty = allow all.
    pub allowed_public_keys: Vec<String>,
    /// Capabilities excluded from pubkey whitelisting.
    pub excluded_capabilities: Vec<CapabilityExclusion>,
    /// Maximum number of concurrent client sessions (LRU-bounded, default: 1000).
    pub max_sessions: usize,
    /// Session cleanup interval (default: 60s).
    pub cleanup_interval: Duration,
    /// Session timeout (default: 300s).
    pub session_timeout: Duration,
    /// Correlation-retention TTL for server-side event routes (default: 60s).
    ///
    /// Stale route entries older than this are swept from the correlation store.
    /// This prevents leaks -- rmcp owns actual request timeout and cancellation.
    /// Keep this value above your rmcp request timeout to avoid premature cleanup.
    pub request_timeout: Duration,
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
            max_sessions: session_store::DEFAULT_MAX_SESSIONS,
            cleanup_interval: Duration::from_secs(60),
            session_timeout: Duration::from_secs(300),
            request_timeout: Duration::from_secs(60),
        }
    }
}

/// Server-side Nostr transport — receives MCP requests and sends responses.
pub struct NostrServerTransport {
    /// Relay pool for publishing and subscribing.
    base: BaseTransport,
    /// Configuration for this server transport.
    config: NostrServerTransportConfig,
    /// Manages tag composition and publishing for CEP-6 announcements and CEP-35 discovery.
    announcement_manager: announcement_manager::AnnouncementManager,
    /// Client sessions.
    sessions: SessionStore,
    /// Reverse lookup: event_id → client route.
    event_routes: ServerEventRouteStore,
    /// CEP-19: Track the incoming gift-wrap kind per request for mirroring.
    request_wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>>,
    /// Outer gift-wrap event IDs successfully decrypted and verified (inner `verify()`).
    /// Duplicate outer ids are skipped before decrypt; ids are inserted only after success
    /// so failed decrypt/verify can be retried on redelivery.
    seen_gift_wrap_ids: Arc<Mutex<LruCache<EventId, ()>>>,
    /// Channel for incoming MCP messages (consumed by the MCP server).
    message_tx: Option<tokio::sync::mpsc::UnboundedSender<IncomingRequest>>,
    message_rx: Option<tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>>,
    /// Token used to cancel spawned tasks (event loop + cleanup) on close().
    cancellation_token: CancellationToken,
    /// Handles for spawned tasks (event loop + cleanup).
    task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl NostrServerTransportConfig {
    /// Set the encryption mode.
    pub fn with_encryption_mode(mut self, mode: EncryptionMode) -> Self {
        self.encryption_mode = mode;
        self
    }
    /// Set the gift-wrap mode (CEP-19).
    pub fn with_gift_wrap_mode(mut self, mode: GiftWrapMode) -> Self {
        self.gift_wrap_mode = mode;
        self
    }
    /// Set server information for announcements.
    pub fn with_server_info(mut self, info: ServerInfo) -> Self {
        self.server_info = Some(info);
        self
    }
    /// Enable or disable public announcement publishing (CEP-6).
    pub fn with_announced_server(mut self, announced: bool) -> Self {
        self.is_announced_server = announced;
        self
    }
    /// Set the allowed client public keys (hex). Empty = allow all.
    pub fn with_allowed_public_keys(mut self, keys: Vec<String>) -> Self {
        self.allowed_public_keys = keys;
        self
    }
    /// Set capabilities excluded from pubkey whitelisting.
    pub fn with_excluded_capabilities(mut self, caps: Vec<CapabilityExclusion>) -> Self {
        self.excluded_capabilities = caps;
        self
    }
    /// Set the maximum number of concurrent client sessions.
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max;
        self
    }
    /// Set the relay URLs to connect to.
    pub fn with_relay_urls(mut self, urls: Vec<String>) -> Self {
        self.relay_urls = urls;
        self
    }
    /// Set the session cleanup interval.
    pub fn with_cleanup_interval(mut self, interval: Duration) -> Self {
        self.cleanup_interval = interval;
        self
    }
    /// Set the session timeout.
    pub fn with_session_timeout(mut self, timeout: Duration) -> Self {
        self.session_timeout = timeout;
        self
    }
    /// Set the correlation-retention TTL for event routes.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }
}

/// An incoming MCP request with metadata for routing the response.
#[derive(Debug)]
#[non_exhaustive]
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
        let relay_pool: Arc<dyn RelayPoolTrait> =
            Arc::new(RelayPool::new(signer).await.map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    "Failed to initialize relay pool for server transport"
                );
                error
            })?);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let seen_gift_wrap_ids = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"),
        )));

        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.relay_urls.len(),
            announced = config.is_announced_server,
            encryption_mode = ?config.encryption_mode,
            gift_wrap_mode = ?config.gift_wrap_mode,
            "Created server transport"
        );
        Ok(Self {
            announcement_manager: announcement_manager::AnnouncementManager::new(
                Arc::clone(&relay_pool),
                config.server_info.clone(),
                config.encryption_mode,
                config.gift_wrap_mode,
            ),
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            sessions: SessionStore::with_capacity(config.max_sessions),
            config,
            event_routes: ServerEventRouteStore::new(),
            request_wrap_kinds: Arc::new(RwLock::new(HashMap::new())),
            seen_gift_wrap_ids,
            message_tx: Some(tx),
            message_rx: Some(rx),
            cancellation_token: CancellationToken::new(),
            task_handles: Vec::new(),
        })
    }

    /// Like [`new`](Self::new) but accepts an existing relay pool.
    pub async fn with_relay_pool(
        config: NostrServerTransportConfig,
        relay_pool: Arc<dyn RelayPoolTrait>,
    ) -> Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let seen_gift_wrap_ids = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"),
        )));

        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.relay_urls.len(),
            announced = config.is_announced_server,
            encryption_mode = ?config.encryption_mode,
            "Created server transport (with_relay_pool)"
        );
        Ok(Self {
            announcement_manager: announcement_manager::AnnouncementManager::new(
                Arc::clone(&relay_pool),
                config.server_info.clone(),
                config.encryption_mode,
                config.gift_wrap_mode,
            ),
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            sessions: SessionStore::with_capacity(config.max_sessions),
            config,
            request_wrap_kinds: Arc::new(RwLock::new(HashMap::new())),
            event_routes: ServerEventRouteStore::new(),
            seen_gift_wrap_ids,
            message_tx: Some(tx),
            message_rx: Some(rx),
            cancellation_token: CancellationToken::new(),
            task_handles: Vec::new(),
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

        // Spawn event loop with cancellation support
        let relay_pool = Arc::clone(&self.base.relay_pool);
        let sessions = self.sessions.clone();
        let event_routes = self.event_routes.clone();
        let request_wrap_kinds = self.request_wrap_kinds.clone();
        let tx = self
            .message_tx
            .as_ref()
            .expect("message_tx must exist before start()")
            .clone();
        let allowed = self.config.allowed_public_keys.clone();
        let excluded = self.config.excluded_capabilities.clone();
        let encryption_mode = self.config.encryption_mode;
        let gift_wrap_mode = self.config.gift_wrap_mode;
        let is_announced_server = self.config.is_announced_server;
        let common_tags_snapshot = self.announcement_manager.common_tags_snapshot();
        let seen_gift_wrap_ids = self.seen_gift_wrap_ids.clone();
        let event_loop_token = self.cancellation_token.child_token();

        let event_loop_handle = tokio::spawn(async move {
            Self::event_loop(
                relay_pool,
                sessions,
                event_routes,
                request_wrap_kinds,
                tx,
                allowed,
                excluded,
                encryption_mode,
                gift_wrap_mode,
                is_announced_server,
                common_tags_snapshot,
                seen_gift_wrap_ids,
                event_loop_token,
            )
            .await;
        });

        // Spawn session cleanup with cancellation support
        let sessions_cleanup = self.sessions.clone();
        let event_routes_cleanup = self.event_routes.clone();
        let request_wrap_kinds_cleanup = self.request_wrap_kinds.clone();
        let cleanup_interval = self.config.cleanup_interval;
        let session_timeout = self.config.session_timeout;
        let request_timeout = self.config.request_timeout;
        let cleanup_token = self.cancellation_token.child_token();

        let cleanup_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_interval);
            loop {
                tokio::select! {
                    _ = cleanup_token.cancelled() => {
                        tracing::info!(
                            target: LOG_TARGET,
                            "Server cleanup task cancelled"
                        );
                        break;
                    }
                    _ = interval.tick() => {
                        let cleaned = Self::cleanup_sessions(
                            &sessions_cleanup,
                            &event_routes_cleanup,
                            &request_wrap_kinds_cleanup,
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
                }

                // Sweep stale route entries in active sessions (rmcp handles timeout errors).
                let swept_event_ids = event_routes_cleanup
                    .sweep_stale_routes(request_timeout)
                    .await;
                if !swept_event_ids.is_empty() {
                    let mut kinds_w = request_wrap_kinds_cleanup.write().await;
                    for event_id in &swept_event_ids {
                        kinds_w.remove(event_id);
                    }
                    drop(kinds_w);
                    tracing::warn!(
                        target: LOG_TARGET,
                        swept = swept_event_ids.len(),
                        timeout_secs = request_timeout.as_secs(),
                        "Swept stale event routes (rmcp handles timeout errors)"
                    );
                }
            }
        });

        self.task_handles.push(event_loop_handle);
        self.task_handles.push(cleanup_handle);

        tracing::info!(
            target: LOG_TARGET,
            relay_count = self.config.relay_urls.len(),
            cleanup_interval_secs = self.config.cleanup_interval.as_secs(),
            session_timeout_secs = self.config.session_timeout.as_secs(),
            "Server transport loops spawned"
        );
        Ok(())
    }

    /// Close the transport — cancels event loop and cleanup tasks, then disconnects.
    pub async fn close(&mut self) -> Result<()> {
        self.cancellation_token.cancel();
        for handle in self.task_handles.drain(..) {
            let _ = handle.await;
        }
        self.message_tx.take();
        self.base.disconnect().await?;
        self.sessions.clear().await;
        self.event_routes.clear().await;
        Ok(())
    }

    /// Send a response back to the client that sent the original request.
    pub async fn send_response(&self, event_id: &str, mut response: JsonRpcMessage) -> Result<()> {
        // Consume the route up-front so only one concurrent responder can proceed
        // for a given event_id.
        let route = self.event_routes.pop(event_id).await.ok_or_else(|| {
            tracing::error!(
                target: LOG_TARGET,
                event_id = %event_id,
                "No client found for response correlation"
            );
            Error::Other(format!("No client found for event {event_id}"))
        })?;

        let client_pubkey_hex = route.client_pubkey;
        let original_request_id = route.original_request_id;
        let progress_token = route.progress_token;

        let mut sessions_w = self.sessions.write().await;
        let session = sessions_w.get_mut(&client_pubkey_hex).ok_or_else(|| {
            tracing::error!(
                target: LOG_TARGET,
                client_pubkey = %client_pubkey_hex,
                "No session for correlated client"
            );
            Error::Other(format!("No session for client {client_pubkey_hex}"))
        })?;

        // Restore original request ID
        match &mut response {
            JsonRpcMessage::Response(r) => r.id = original_request_id.clone(),
            JsonRpcMessage::ErrorResponse(r) => r.id = original_request_id.clone(),
            _ => {}
        }

        let is_encrypted = session.is_encrypted;

        // CEP-35: include discovery tags on first response to this client
        let discovery_tags = self.take_pending_server_discovery_tags(session);
        drop(sessions_w);

        // CEP-19: Look up the incoming wrap kind for mirroring
        let mirrored_wrap_kind = self
            .request_wrap_kinds
            .read()
            .await
            .get(event_id)
            .copied()
            .flatten();

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

        let base_tags = BaseTransport::create_response_tags(&client_pubkey, &event_id_parsed);
        let tags = BaseTransport::compose_outbound_tags(&base_tags, &discovery_tags, &[]);

        if let Err(error) = self
            .base
            .send_mcp_message(
                &response,
                &client_pubkey,
                CTXVM_MESSAGES_KIND,
                tags,
                Some(is_encrypted),
                Self::select_outbound_gift_wrap_kind(
                    self.config.gift_wrap_mode,
                    is_encrypted,
                    mirrored_wrap_kind,
                ),
            )
            .await
        {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                client_pubkey = %client_pubkey_hex,
                event_id = %event_id,
                "Failed to publish response message"
            );

            // Re-register route on publish failure so caller can retry.
            self.event_routes
                .register(
                    event_id.to_string(),
                    client_pubkey_hex,
                    original_request_id,
                    progress_token,
                )
                .await;

            return Err(error);
        }

        // Clean up wrap-kind tracking
        self.request_wrap_kinds.write().await.remove(event_id);

        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&client_pubkey_hex) {
            // Clean up progress token
            if let Some(token) = progress_token {
                session.pending_requests.remove(&token);
            }
            session.event_to_progress_token.remove(event_id);
            session.pending_requests.remove(event_id);
        }
        drop(sessions);

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
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(client_pubkey_hex)
            .ok_or_else(|| Error::Other(format!("No session for {client_pubkey_hex}")))?;
        let is_encrypted = session.is_encrypted;
        let supports_ephemeral = session.supports_ephemeral_gift_wrap;

        // CEP-35: include discovery tags on first message to this client
        let discovery_tags = self.take_pending_server_discovery_tags(session);
        drop(sessions);

        let client_pubkey =
            PublicKey::from_hex(client_pubkey_hex).map_err(|e| Error::Other(e.to_string()))?;

        let mut base_tags = BaseTransport::create_recipient_tags(&client_pubkey);
        if let Some(eid) = correlated_event_id {
            let event_id = EventId::from_hex(eid).map_err(|e| Error::Other(e.to_string()))?;
            base_tags.push(Tag::event(event_id));
        }

        let tags = BaseTransport::compose_outbound_tags(&base_tags, &discovery_tags, &[]);

        // CEP-19: Look up mirrored wrap kind from correlated request
        let correlated_wrap_kind = if let Some(event_id) = correlated_event_id {
            self.request_wrap_kinds
                .read()
                .await
                .get(event_id)
                .copied()
                .flatten()
        } else {
            None
        };

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
                    correlated_wrap_kind,
                    supports_ephemeral,
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
        self.announcement_manager.set_extra_common_tags(tags);
    }

    /// Sets pricing tags to include in announcement/list events and capability list responses.
    pub fn set_announcement_pricing_tags(&mut self, tags: Vec<Tag>) {
        self.announcement_manager.set_pricing_tags(tags);
    }

    /// Publish server announcement (kind 11316).
    pub async fn announce(&self) -> Result<EventId> {
        self.announcement_manager.announce().await
    }

    /// Publish tools list (kind 11317).
    pub async fn publish_tools(&self, tools: Vec<serde_json::Value>) -> Result<EventId> {
        self.announcement_manager.publish_tools(tools).await
    }

    /// Publish resources list (kind 11318).
    pub async fn publish_resources(&self, resources: Vec<serde_json::Value>) -> Result<EventId> {
        self.announcement_manager.publish_resources(resources).await
    }

    /// Publish prompts list (kind 11320).
    pub async fn publish_prompts(&self, prompts: Vec<serde_json::Value>) -> Result<EventId> {
        self.announcement_manager.publish_prompts(prompts).await
    }

    /// Publish resource templates list (kind 11319).
    pub async fn publish_resource_templates(
        &self,
        templates: Vec<serde_json::Value>,
    ) -> Result<EventId> {
        self.announcement_manager
            .publish_resource_templates(templates)
            .await
    }

    /// Delete server announcements (NIP-09 kind 5).
    pub async fn delete_announcements(&self, reason: &str) -> Result<()> {
        self.announcement_manager.delete_announcements(reason).await
    }

    /// Publish tools list from rmcp typed tool descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_tools_typed(&self, tools: Vec<rmcp::model::Tool>) -> Result<EventId> {
        self.announcement_manager.publish_tools_typed(tools).await
    }

    /// Publish resources list from rmcp typed resource descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_resources_typed(
        &self,
        resources: Vec<rmcp::model::Resource>,
    ) -> Result<EventId> {
        self.announcement_manager
            .publish_resources_typed(resources)
            .await
    }

    /// Publish prompts list from rmcp typed prompt descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_prompts_typed(
        &self,
        prompts: Vec<rmcp::model::Prompt>,
    ) -> Result<EventId> {
        self.announcement_manager
            .publish_prompts_typed(prompts)
            .await
    }

    /// Publish resource templates list from rmcp typed template descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_resource_templates_typed(
        &self,
        templates: Vec<rmcp::model::ResourceTemplate>,
    ) -> Result<EventId> {
        self.announcement_manager
            .publish_resource_templates_typed(templates)
            .await
    }

    // ── CEP-35 discovery tag helpers ──────────────────────────────

    /// One-shot: returns common tags if not yet sent to this client, empty otherwise.
    fn take_pending_server_discovery_tags(&self, session: &mut ClientSession) -> Vec<Tag> {
        if session.has_sent_common_tags {
            return vec![];
        }
        session.has_sent_common_tags = true;
        self.announcement_manager.get_common_tags()
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

    #[allow(clippy::too_many_arguments)]
    async fn event_loop(
        relay_pool: Arc<dyn RelayPoolTrait>,
        sessions: SessionStore,
        event_routes: ServerEventRouteStore,
        request_wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>>,
        tx: tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
        allowed_pubkeys: Vec<String>,
        excluded_capabilities: Vec<CapabilityExclusion>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        is_announced_server: bool,
        common_tags_snapshot: announcement_manager::CommonTagsSnapshot,
        seen_gift_wrap_ids: Arc<Mutex<LruCache<EventId, ()>>>,
        cancel: CancellationToken,
    ) {
        let mut notifications = relay_pool.notifications();

        loop {
            let notification = tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(
                        target: LOG_TARGET,
                        "Server event loop cancelled"
                    );
                    break;
                }
                result = notifications.recv() => {
                    match result {
                        Ok(n) => n,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                target: LOG_TARGET,
                                skipped = n,
                                "Relay broadcast lagged, skipping missed events"
                            );
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            };
            if let RelayPoolNotification::Event { event, .. } = notification {
                let is_gift_wrap = event.kind == Kind::Custom(GIFT_WRAP_KIND)
                    || event.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND);
                let outer_kind: u16 = event.kind.as_u16();

                // CEP-19: Drop gift-wraps that violate the configured gift-wrap mode
                if is_gift_wrap && !gift_wrap_mode.allows_kind(outer_kind) {
                    tracing::warn!(
                        target: LOG_TARGET,
                        event_id = %event.id.to_hex(),
                        event_kind = outer_kind,
                        configured_mode = ?gift_wrap_mode,
                        "Dropping gift-wrap because it violates gift_wrap_mode policy"
                    );
                    continue;
                }

                let (content, sender_pubkey, event_id, is_encrypted, inner_tags) = if is_gift_wrap {
                    if encryption_mode == EncryptionMode::Disabled {
                        tracing::warn!(
                            target: LOG_TARGET,
                            event_id = %event.id.to_hex(),
                            sender_pubkey = %event.pubkey.to_hex(),
                            "Received encrypted message but encryption is disabled"
                        );
                        continue;
                    }
                    {
                        let guard = match seen_gift_wrap_ids.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        if guard.contains(&event.id) {
                            tracing::debug!(
                                target: LOG_TARGET,
                                event_id = %event.id.to_hex(),
                                "Skipping duplicate gift-wrap (outer id)"
                            );
                            continue;
                        }
                    }
                    // Single-layer NIP-44 decrypt (matches JS/TS SDK)
                    let signer = match relay_pool.signer().await {
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
                                    {
                                        let mut guard = match seen_gift_wrap_ids.lock() {
                                            Ok(g) => g,
                                            Err(poisoned) => poisoned.into_inner(),
                                        };
                                        guard.put(event.id, ());
                                    }
                                    let inner_tags: Vec<Tag> = inner.tags.to_vec();
                                    (
                                        inner.content,
                                        inner.pubkey.to_hex(),
                                        inner.id.to_hex(),
                                        true,
                                        inner_tags,
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
                        event.tags.to_vec(),
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

                        // Send a JSON-RPC error back for Request messages so the
                        // client doesn't hang indefinitely (announced servers only).
                        if is_announced_server {
                            if let JsonRpcMessage::Request(ref req) = mcp_msg {
                                if let Ok(client_pk) = PublicKey::from_hex(&sender_pubkey) {
                                    let event_id_parsed = EventId::from_hex(&event_id)
                                        .unwrap_or(EventId::all_zeros());
                                    let mut tags = BaseTransport::create_response_tags(
                                        &client_pk,
                                        &event_id_parsed,
                                    );

                                    // CEP-19: Inject common discovery tags on first response
                                    let has_sent = sessions
                                        .get_session(&sender_pubkey)
                                        .await
                                        .is_some_and(|s| s.has_sent_common_tags);
                                    if !has_sent {
                                        common_tags_snapshot.append_common_response_tags(&mut tags);
                                        sessions.mark_common_tags_sent(&sender_pubkey).await;
                                    }

                                    let error_response =
                                        JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
                                            jsonrpc: "2.0".to_string(),
                                            id: req.id.clone(),
                                            error: JsonRpcError {
                                                code: -32000,
                                                message: "Unauthorized".to_string(),
                                                data: None,
                                            },
                                        });

                                    let base = BaseTransport {
                                        relay_pool: Arc::clone(&relay_pool),
                                        encryption_mode,
                                        is_connected: true,
                                    };
                                    if let Err(e) = base
                                        .send_mcp_message(
                                            &error_response,
                                            &client_pk,
                                            CTXVM_MESSAGES_KIND,
                                            tags,
                                            Some(is_encrypted),
                                            Self::select_outbound_gift_wrap_kind(
                                                gift_wrap_mode,
                                                is_encrypted,
                                                if is_gift_wrap { Some(outer_kind) } else { None },
                                            ),
                                        )
                                        .await
                                    {
                                        tracing::error!(
                                            target: LOG_TARGET,
                                            error = %e,
                                            sender_pubkey = %sender_pubkey,
                                            "Failed to send unauthorized error response"
                                        );
                                    }
                                }
                            }
                        } // if is_announced_server

                        continue;
                    }
                }

                // Session management
                let on_evicted_cb = sessions.eviction_callback();
                let mut sessions_w = sessions.write().await;
                if !sessions_w.contains(&sender_pubkey) {
                    let evicted =
                        sessions_w.push(sender_pubkey.clone(), ClientSession::new(is_encrypted));
                    SessionStore::handle_eviction(
                        &sender_pubkey,
                        evicted,
                        &mut sessions_w,
                        on_evicted_cb.as_ref(),
                        &event_routes,
                    )
                    .await;
                }
                let session = sessions_w.get_mut(&sender_pubkey).unwrap();
                session.update_activity();
                session.is_encrypted = is_encrypted;

                // CEP-19: Mark ephemeral support if client used kind 21059
                if is_gift_wrap && outer_kind == EPHEMERAL_GIFT_WRAP_KIND {
                    session.supports_ephemeral_gift_wrap = true;
                }

                // CEP-35: learn client capabilities from inner event tags
                let discovered = learn_peer_capabilities(&inner_tags);
                session.supports_encryption |= discovered.supports_encryption;
                session.supports_ephemeral_encryption |= discovered.supports_ephemeral_encryption;
                // Only learn oversized support if CEP-22 is enabled on this server
                // TODO: wire from config when CEP-22 lands
                let oversized_enabled = false;
                session.supports_oversized_transfer |=
                    oversized_enabled && discovered.supports_oversized_transfer;

                // Track request for correlation
                if let JsonRpcMessage::Request(ref req) = mcp_msg {
                    let original_id = req.id.clone();

                    // Extract progress token from _meta if present.
                    let progress_token = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                        .and_then(|t| t.as_str())
                        .map(String::from);

                    // Duplicate into session fields (kept for backward compat).
                    session
                        .pending_requests
                        .insert(event_id.clone(), original_id.clone());
                    if let Some(ref token) = progress_token {
                        session
                            .pending_requests
                            .insert(token.clone(), serde_json::json!(event_id));
                        session
                            .event_to_progress_token
                            .insert(event_id.clone(), token.clone());
                    }

                    drop(sessions_w);

                    // CEP-19: Record the incoming wrap kind for response mirroring
                    {
                        let mut kinds_w = request_wrap_kinds.write().await;
                        kinds_w.insert(
                            event_id.clone(),
                            if is_gift_wrap { Some(outer_kind) } else { None },
                        );
                    }

                    event_routes
                        .register(
                            event_id.clone(),
                            sender_pubkey.clone(),
                            original_id,
                            progress_token,
                        )
                        .await;
                } else {
                    drop(sessions_w);
                }

                // Handle initialized notification (re-acquire for write)
                if let JsonRpcMessage::Notification(ref n) = mcp_msg {
                    if n.method == "notifications/initialized" {
                        let mut sessions_w2 = sessions.write().await;
                        if let Some(session) = sessions_w2.get_mut(&sender_pubkey) {
                            session.is_initialized = true;
                        }
                    }
                }

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
        sessions: &SessionStore,
        event_routes: &ServerEventRouteStore,
        request_wrap_kinds: &Arc<RwLock<HashMap<String, Option<u16>>>>,
        timeout: Duration,
    ) -> usize {
        let mut sessions_w = sessions.write().await;
        let mut cleaned = 0;
        let mut stale_event_ids = Vec::new();

        // LruCache has no retain(); collect expired keys then pop each one.
        let expired_keys: Vec<String> = sessions_w
            .iter()
            .filter(|(_, session)| session.last_activity.elapsed() > timeout)
            .map(|(k, _)| k.clone())
            .collect();

        for key in &expired_keys {
            if let Some(session) = sessions_w.pop(key) {
                stale_event_ids.extend(session.pending_requests.keys().cloned());
                stale_event_ids.extend(session.event_to_progress_token.keys().cloned());
                tracing::debug!(
                    target: LOG_TARGET,
                    client_pubkey = %key,
                    "Session expired"
                );
                cleaned += 1;
            }
        }
        drop(sessions_w);

        {
            let mut kinds_w = request_wrap_kinds.write().await;
            for event_id in &stale_event_ids {
                kinds_w.remove(event_id);
            }
        }

        for event_id in &stale_event_ids {
            event_routes.pop(event_id).await;
        }

        cleaned
    }

    /// CEP-19: Choose outbound gift-wrap kind for responses.
    /// If `is_encrypted` is false, return None (send plaintext).
    /// Otherwise mirror the kind used by the client, falling back to the mode default.
    fn select_outbound_gift_wrap_kind(
        mode: GiftWrapMode,
        is_encrypted: bool,
        mirrored_kind: Option<u16>,
    ) -> Option<u16> {
        if !is_encrypted {
            return None;
        }
        if let Some(kind) = mirrored_kind {
            if mode.allows_kind(kind) {
                return Some(kind);
            }
        }
        match mode {
            GiftWrapMode::Persistent => Some(GIFT_WRAP_KIND),
            GiftWrapMode::Ephemeral => Some(EPHEMERAL_GIFT_WRAP_KIND),
            GiftWrapMode::Optional => Some(GIFT_WRAP_KIND),
        }
    }

    /// CEP-19: Choose outbound gift-wrap kind for notifications.
    fn select_outbound_notification_gift_wrap_kind(
        mode: GiftWrapMode,
        is_encrypted: bool,
        correlated_wrap_kind: Option<u16>,
        client_supports_ephemeral: bool,
    ) -> Option<u16> {
        if !is_encrypted {
            return None;
        }
        // Mirror correlated request kind if available
        if let Some(kind) = correlated_wrap_kind {
            if mode.allows_kind(kind) {
                return Some(kind);
            }
        }
        // Fall back based on learned ephemeral support
        if client_supports_ephemeral && mode.supports_ephemeral() {
            return Some(EPHEMERAL_GIFT_WRAP_KIND);
        }
        match mode {
            GiftWrapMode::Persistent => Some(GIFT_WRAP_KIND),
            GiftWrapMode::Ephemeral => Some(EPHEMERAL_GIFT_WRAP_KIND),
            GiftWrapMode::Optional => Some(GIFT_WRAP_KIND),
        }
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
        assert!(!session.has_sent_common_tags);
        assert!(!session.supports_ephemeral_gift_wrap);
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
        let sessions = SessionStore::new();
        let event_routes = ServerEventRouteStore::new();

        // Insert a session with an old activity time
        let mut session = ClientSession::new(false);
        session
            .pending_requests
            .insert("evt1".to_string(), serde_json::json!(1));
        sessions.write().await.put("pubkey1".to_string(), session);
        event_routes
            .register(
                "evt1".to_string(),
                "pubkey1".to_string(),
                serde_json::json!(1),
                None,
            )
            .await;

        let request_wrap_kinds = Arc::new(RwLock::new(HashMap::new()));

        // With a long timeout, nothing should be cleaned
        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_routes,
            &request_wrap_kinds,
            Duration::from_secs(300),
        )
        .await;
        assert_eq!(cleaned, 0);
        assert_eq!(sessions.session_count().await, 1);

        // With zero timeout, it should be cleaned
        thread::sleep(Duration::from_millis(5));
        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_routes,
            &request_wrap_kinds,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(cleaned, 1);
        assert_eq!(sessions.session_count().await, 0);
        assert!(event_routes.pop("evt1").await.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_preserves_active_sessions() {
        let sessions = SessionStore::new();
        let event_routes = ServerEventRouteStore::new();
        let request_wrap_kinds = Arc::new(RwLock::new(HashMap::new()));

        sessions
            .get_or_create_session("active", false, &event_routes)
            .await;

        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_routes,
            &request_wrap_kinds,
            Duration::from_secs(300),
        )
        .await;
        assert_eq!(cleaned, 0);
        assert_eq!(sessions.session_count().await, 1);
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
        assert_eq!(config.gift_wrap_mode, GiftWrapMode::Optional);
        assert!(config.allowed_public_keys.is_empty());
        assert!(config.excluded_capabilities.is_empty());
        assert_eq!(config.max_sessions, 1000);
        assert_eq!(config.cleanup_interval, Duration::from_secs(60));
        assert_eq!(config.session_timeout, Duration::from_secs(300));
        assert_eq!(config.request_timeout, Duration::from_secs(60));
        assert!(config.server_info.is_none());
    }

    // ── CEP-19 helper logic ──────────────────────────────────────

    #[test]
    fn test_select_outbound_gift_wrap_kind_plaintext() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Optional,
                false,
                Some(GIFT_WRAP_KIND),
            ),
            None
        );
    }

    #[test]
    fn test_select_outbound_gift_wrap_kind_mirrors_incoming() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                Some(EPHEMERAL_GIFT_WRAP_KIND),
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_gift_wrap_kind_persistent_mode_overrides_ephemeral() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Persistent,
                true,
                Some(EPHEMERAL_GIFT_WRAP_KIND),
            ),
            Some(GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_append_common_response_tags_includes_encryption_when_optional() {
        let snapshot = announcement_manager::CommonTagsSnapshot {
            server_info: None,
            extra_common_tags: vec![],
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
        };
        let mut tags = Vec::new();
        snapshot.append_common_response_tags(&mut tags);
        let kinds: Vec<String> = tags.iter().map(|t| format!("{:?}", t.kind())).collect();
        assert!(
            kinds.iter().any(|k| k.contains("support_encryption")),
            "should include support_encryption tag"
        );
    }

    #[test]
    fn test_append_common_response_tags_no_encryption_when_disabled() {
        let snapshot = announcement_manager::CommonTagsSnapshot {
            server_info: None,
            extra_common_tags: vec![],
            encryption_mode: EncryptionMode::Disabled,
            gift_wrap_mode: GiftWrapMode::Optional,
        };
        let mut tags = Vec::new();
        snapshot.append_common_response_tags(&mut tags);
        assert!(
            tags.is_empty(),
            "should not include encryption tags when encryption disabled"
        );
    }

    #[test]
    fn test_select_outbound_notification_gift_wrap_kind_plaintext() {
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Optional,
                false,
                Some(EPHEMERAL_GIFT_WRAP_KIND),
                true,
            ),
            None
        );
    }

    #[test]
    fn test_select_outbound_notification_gift_wrap_kind_mirrors_correlated() {
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                Some(EPHEMERAL_GIFT_WRAP_KIND),
                false,
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_notification_gift_wrap_kind_falls_back_to_mode_if_correlated_not_allowed(
    ) {
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Ephemeral,
                true,
                Some(GIFT_WRAP_KIND),
                false,
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_notification_gift_wrap_kind_uses_ephemeral_if_supported() {
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                None,
                true,
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_notification_gift_wrap_kind_uses_persistent_if_ephemeral_supported_but_mode_persistent(
    ) {
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Persistent,
                true,
                None,
                true,
            ),
            Some(GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_notification_gift_wrap_kind_uses_default_mode_if_ephemeral_not_supported(
    ) {
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                None,
                false,
            ),
            Some(GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_append_common_response_tags_includes_ephemeral_tag() {
        let snapshot = announcement_manager::CommonTagsSnapshot {
            server_info: None,
            extra_common_tags: vec![],
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
        };
        let mut tags = Vec::new();
        snapshot.append_common_response_tags(&mut tags);
        let kinds: Vec<String> = tags.iter().map(|t| format!("{:?}", t.kind())).collect();
        assert!(
            kinds
                .iter()
                .any(|k| k.contains("support_encryption_ephemeral")),
            "should include support_encryption_ephemeral tag"
        );
    }

    #[test]
    fn test_append_common_response_tags_includes_server_info() {
        let server_info = ServerInfo {
            name: Some("TestServer".to_string()),
            ..Default::default()
        };
        let snapshot = announcement_manager::CommonTagsSnapshot {
            server_info: Some(server_info),
            extra_common_tags: vec![],
            encryption_mode: EncryptionMode::Disabled,
            gift_wrap_mode: GiftWrapMode::Optional,
        };
        let mut tags = Vec::new();
        snapshot.append_common_response_tags(&mut tags);
        let tag_value = tags
            .iter()
            .find(|t| (*t).clone().to_vec().first().map(|s| s.as_str()) == Some("name"))
            .and_then(|t| t.clone().to_vec().get(1).cloned());
        assert_eq!(tag_value.as_deref(), Some("TestServer"));
    }

    #[test]
    fn test_append_common_response_tags_extra_tags() {
        let extra_tags = vec![Tag::custom(
            TagKind::Custom("custom_tag".into()),
            vec!["value".to_string()],
        )];
        let snapshot = announcement_manager::CommonTagsSnapshot {
            server_info: None,
            extra_common_tags: extra_tags,
            encryption_mode: EncryptionMode::Disabled,
            gift_wrap_mode: GiftWrapMode::Optional,
        };
        let mut tags = Vec::new();
        snapshot.append_common_response_tags(&mut tags);
        let tag_value = tags
            .iter()
            .find(|t| (*t).clone().to_vec().first().map(|s| s.as_str()) == Some("custom_tag"))
            .and_then(|t| t.clone().to_vec().get(1).cloned());
        assert_eq!(tag_value.as_deref(), Some("value"));
    }

    // ── CEP-35 discovery tag helpers ────────────────────────────

    #[test]
    fn test_cep35_client_session_new_fields_default_false() {
        let session = ClientSession::new(false);
        assert!(!session.has_sent_common_tags);
        assert!(!session.supports_encryption);
        assert!(!session.supports_ephemeral_encryption);
        assert!(!session.supports_oversized_transfer);
    }

    #[test]
    fn test_cep35_capability_or_assign() {
        let mut session = ClientSession::new(false);

        session.supports_encryption |= true;
        session.supports_ephemeral_encryption |= false;

        session.supports_encryption |= false;
        session.supports_ephemeral_encryption |= true;

        assert!(session.supports_encryption, "OR-assign must not downgrade");
        assert!(session.supports_ephemeral_encryption);
        assert!(!session.supports_oversized_transfer);
    }

    #[test]
    fn test_config_gift_wrap_mode_default() {
        let config = NostrServerTransportConfig::default();
        assert_eq!(config.gift_wrap_mode, GiftWrapMode::Optional);
    }
}
