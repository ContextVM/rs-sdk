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
use crate::transport::oversized_transfer::{
    build_oversized_frames, resolve_safe_chunk_size, OversizedFrame, OversizedSenderOptions,
    OversizedTransferConfig, OversizedTransferReceiver, TransferPolicy, ACCEPT_PROGRESS,
};

const LOG_TARGET: &str = "contextvm_sdk::transport::server";

/// CEP-22: the `support_oversized_transfer` capability tags to advertise, or
/// empty when oversized transfer is disabled.
fn oversized_support_tags(config: &NostrServerTransportConfig) -> Vec<Tag> {
    if config.oversized_transfer.enabled {
        vec![Tag::custom(
            TagKind::Custom(tags::SUPPORT_OVERSIZED_TRANSFER.into()),
            Vec::<String>::new(),
        )]
    } else {
        Vec::new()
    }
}

/// CEP-22: build the empty per-peer reassembly store, bounded to `max_sessions`
/// peers (one [`OversizedTransferReceiver`] per client pubkey, inserted lazily by
/// the inbound event loop).
fn new_oversized_receiver_store(
    max_sessions: usize,
) -> Arc<RwLock<LruCache<String, OversizedTransferReceiver>>> {
    Arc::new(RwLock::new(LruCache::new(
        NonZeroUsize::new(max_sessions).unwrap_or(NonZeroUsize::new(1).unwrap()),
    )))
}

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
    /// Explicit relay URLs to advertise in kind 10002 (NIP-65 relay list).
    ///
    /// Falls back to the transport's `relay_urls` when omitted.
    pub relay_list_urls: Option<Vec<String>>,
    /// Additional publication targets for discoverability events.
    ///
    /// Merged with `relay_list_urls` when computing where to send events.
    /// Defaults to [`DEFAULT_BOOTSTRAP_RELAY_URLS`] when omitted.
    pub bootstrap_relay_urls: Option<Vec<String>>,
    /// Whether to publish a relay list event (kind 10002). Default: `true`.
    pub publish_relay_list: bool,
    /// Optional NIP-01 profile metadata (kind 0) to publish at startup.
    pub profile_metadata: Option<ProfileMetadata>,
    /// CEP-22 oversized payload transfer configuration. Disabled by default.
    pub oversized_transfer: OversizedTransferConfig,
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
            relay_list_urls: None,
            bootstrap_relay_urls: None,
            publish_relay_list: true,
            profile_metadata: None,
            oversized_transfer: OversizedTransferConfig::default(),
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
    /// CEP-22: per-peer reassembly engines for inbound oversized transfers, keyed
    /// by client pubkey (hex) and bounded to `max_sessions` peers. Each receiver
    /// enforces the configured per-peer admission policy. Populated by the inbound
    /// event loop; cleared on [`close`](Self::close).
    oversized_receiver: Arc<RwLock<LruCache<String, OversizedTransferReceiver>>>,
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
    /// Set explicit relay URLs to advertise in the relay list event (kind 10002).
    pub fn with_relay_list_urls(mut self, urls: Vec<String>) -> Self {
        self.relay_list_urls = Some(urls);
        self
    }
    /// Set additional bootstrap relay URLs for discoverability event publication.
    pub fn with_bootstrap_relay_urls(mut self, urls: Vec<String>) -> Self {
        self.bootstrap_relay_urls = Some(urls);
        self
    }
    /// Enable or disable relay list publication (kind 10002).
    pub fn with_publish_relay_list(mut self, publish: bool) -> Self {
        self.publish_relay_list = publish;
        self
    }
    /// Set NIP-01 profile metadata (kind 0) for publication at startup.
    pub fn with_profile_metadata(mut self, metadata: ProfileMetadata) -> Self {
        self.profile_metadata = Some(metadata);
        self
    }
    /// Set the full CEP-22 oversized payload transfer configuration.
    pub fn with_oversized_transfer(mut self, config: OversizedTransferConfig) -> Self {
        self.oversized_transfer = config;
        self
    }
    /// Enable or disable CEP-22 oversized payload transfer, leaving other knobs at default.
    pub fn with_oversized_enabled(mut self, enabled: bool) -> Self {
        self.oversized_transfer.enabled = enabled;
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
        let mut announcement_manager = announcement_manager::AnnouncementManager::new(
            Arc::clone(&relay_pool),
            config.server_info.clone(),
            config.encryption_mode,
            config.gift_wrap_mode,
            tx.clone(),
            config.relay_urls.clone(),
            config.relay_list_urls.clone(),
            config.bootstrap_relay_urls.clone(),
            config.publish_relay_list,
            config.profile_metadata.clone(),
        );
        // CEP-22: advertise oversized-transfer support in announcements + first responses.
        announcement_manager.set_internal_common_tags(oversized_support_tags(&config));
        Ok(Self {
            announcement_manager,
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            sessions: SessionStore::with_capacity(config.max_sessions),
            oversized_receiver: new_oversized_receiver_store(config.max_sessions),
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
        let mut announcement_manager = announcement_manager::AnnouncementManager::new(
            Arc::clone(&relay_pool),
            config.server_info.clone(),
            config.encryption_mode,
            config.gift_wrap_mode,
            tx.clone(),
            config.relay_urls.clone(),
            config.relay_list_urls.clone(),
            config.bootstrap_relay_urls.clone(),
            config.publish_relay_list,
            config.profile_metadata.clone(),
        );
        // CEP-22: advertise oversized-transfer support in announcements + first responses.
        announcement_manager.set_internal_common_tags(oversized_support_tags(&config));
        Ok(Self {
            announcement_manager,
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            sessions: SessionStore::with_capacity(config.max_sessions),
            oversized_receiver: new_oversized_receiver_store(config.max_sessions),
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
        let oversized_enabled = self.config.oversized_transfer.enabled;
        let oversized_receiver = self.oversized_receiver.clone();
        let transfer_policy: TransferPolicy = (&self.config.oversized_transfer).into();
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
                oversized_enabled,
                oversized_receiver,
                transfer_policy,
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
        self.announcement_manager.shutdown();
        self.message_tx.take();
        self.base.disconnect().await?;
        self.sessions.clear().await;
        self.event_routes.clear().await;
        self.oversized_receiver.write().await.clear();
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

        // CEP-22: serialize once, *after* id restoration. The threshold check and
        // the oversized split must both derive from this exact post-restoration
        // string so the client reassembles bytes whose `id` matches the digest.
        let serialized = serde_json::to_string(&response)?;

        let is_encrypted = session.is_encrypted;
        // CEP-22: capture the peer's oversized support while the session lock is held.
        let supports_oversized_transfer = session.supports_oversized_transfer;

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
        let gift_wrap_kind = Self::select_outbound_gift_wrap_kind(
            self.config.gift_wrap_mode,
            is_encrypted,
            mirrored_wrap_kind,
        );

        // CEP-22: a response is eligible for oversized fragmentation only when the
        // feature is enabled, the peer advertised support, and the request carried a
        // progressToken to address the frames with.
        let oversized_eligible = self.config.oversized_transfer.enabled
            && progress_token.is_some()
            && supports_oversized_transfer;
        let threshold = self.config.oversized_transfer.threshold;

        // CEP-22: relay size limits apply to the *published* Nostr event, so decide
        // on the published byte size, not the raw payload (mirrors TS
        // `measurePublishedMcpMessageSize`). The raw serialized length is a cheap
        // lower bound: at/above the threshold the response is conclusively oversized
        // and we fragment without building a single event — an escape-heavy payload
        // could otherwise overflow NIP-44's plaintext limit while measuring. Below
        // it, build the single event once, measure it, and reuse it when it fits.
        let mut reuse_event: Option<Event> = None;
        let fragment = if !oversized_eligible {
            false
        } else if serialized.len() >= threshold {
            true
        } else {
            match self
                .base
                .prepare_mcp_message(
                    &response,
                    &client_pubkey,
                    CTXVM_MESSAGES_KIND,
                    tags.clone(),
                    Some(is_encrypted),
                    gift_wrap_kind,
                )
                .await
            {
                Ok((_id, publishable)) => {
                    let published_len = serde_json::to_string(&publishable)
                        .map(|s| s.len())
                        .unwrap_or(usize::MAX);
                    if published_len > threshold {
                        true
                    } else {
                        reuse_event = Some(publishable);
                        false
                    }
                }
                // Could not build one event (e.g. NIP-44 plaintext overflow from an
                // escape-heavy payload) → it cannot be sent as a single event.
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        error = %error,
                        event_id = %event_id,
                        "Single-event build failed; sending response as oversized transfer"
                    );
                    true
                }
            }
        };

        // Both paths converge on the cleanup tail below — neither early-returns on
        // success.
        let send_result: Result<()> = if fragment {
            self.send_oversized_response(
                &serialized,
                progress_token.as_deref().unwrap_or_default(),
                &client_pubkey,
                &base_tags,
                tags,
                is_encrypted,
                gift_wrap_kind,
            )
            .await
        } else if let Some(publishable) = reuse_event {
            // Reuse the event already built for the size check — no re-encryption.
            self.base
                .relay_pool
                .publish_event(&publishable)
                .await
                .map(|_| ())
        } else {
            self.base
                .send_mcp_message(
                    &response,
                    &client_pubkey,
                    CTXVM_MESSAGES_KIND,
                    tags,
                    Some(is_encrypted),
                    gift_wrap_kind,
                )
                .await
                .map(|_| ())
        };

        if let Err(error) = send_result {
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

    /// CEP-22: publish a response as an ordered oversized-transfer frame sequence.
    ///
    /// Splits the post-restoration `serialized` string into `start → chunks… →
    /// end` frames (digest and split both derived from that exact string) and
    /// publishes each as a `notifications/progress` event to `recipient`. The
    /// server never reserves the `accept` slot or waits for a handshake — it only
    /// fragments for peers already known to support the feature. One-shot
    /// discovery tags ride the `start` frame only (`start_tags`); every later
    /// frame carries bare recipient + `e`-tags (`base_tags`).
    #[allow(clippy::too_many_arguments)]
    async fn send_oversized_response(
        &self,
        serialized: &str,
        progress_token: &str,
        recipient: &PublicKey,
        base_tags: &[Tag],
        start_tags: Vec<Tag>,
        is_encrypted: bool,
        gift_wrap_kind: Option<u16>,
    ) -> Result<()> {
        // CEP-22: derive a per-chunk payload budget so every published frame stays
        // under the threshold even after the JSON-RPC envelope, signature, and
        // (when encrypted) gift-wrap expansion. Mirrors TS `resolveSafeOversizedChunkSize`.
        // Continuation (chunk) frames carry the response `p`+`e` tags (`base_tags`),
        // so size against those — not the bare recipient tag — or the budget would
        // be ~70 bytes optimistic.
        let chunk_size = resolve_safe_chunk_size(
            self.config.oversized_transfer.chunk_size,
            &self.base,
            recipient,
            base_tags,
            is_encrypted,
            Kind::Custom(gift_wrap_kind.unwrap_or(GIFT_WRAP_KIND)),
            self.config.oversized_transfer.threshold,
        )
        .await?;
        let options = OversizedSenderOptions::new(progress_token).with_chunk_size(chunk_size);
        let frames = build_oversized_frames(serialized, &options)?.into_ordered();

        // Discovery tags ride the start frame; `take` yields them once, then the
        // remaining frames fall back to bare recipient + `e`-tags.
        let mut start_tags = Some(start_tags);
        for frame in frames {
            let tags = start_tags.take().unwrap_or_else(|| base_tags.to_vec());
            let message = JsonRpcMessage::Notification(frame);
            self.base
                .send_mcp_message(
                    &message,
                    recipient,
                    CTXVM_MESSAGES_KIND,
                    tags,
                    Some(is_encrypted),
                    gift_wrap_kind,
                )
                .await?;
        }
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

    /// Read-only snapshot of a client's learned session state, or `None` if no
    /// session exists for that public key (hex). Exposes learned peer
    /// capabilities (encryption, ephemeral encryption, CEP-22 oversized transfer).
    pub async fn session_snapshot(&self, client_pubkey: &str) -> Option<SessionSnapshot> {
        self.sessions.get_session(client_pubkey).await
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

    /// Spawn the CEP-6 auto-publish task if `is_announced_server` is set.
    ///
    /// Called by the rmcp worker after `start()` — not in `start()` itself —
    /// because the auto-publish flow injects synthetic MCP requests that
    /// require an rmcp handler to produce responses.
    #[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
    pub(crate) fn spawn_announcements(&mut self) {
        if self.config.is_announced_server {
            let handle = self
                .announcement_manager
                .spawn_publish_public_announcements(self.cancellation_token.child_token());
            self.task_handles.push(handle);
        }
        // Unconditional: publish profile metadata and relay list (guards inside methods)
        let handle = self.announcement_manager.spawn_publish_discoverability();
        self.task_handles.push(handle);
    }

    /// Forward an announcement response to the announcement manager for publishing.
    ///
    /// Called by the worker when a response with the announcement sentinel ID arrives.
    #[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
    pub(crate) async fn handle_announcement_response(
        &self,
        response: JsonRpcMessage,
    ) -> Result<()> {
        self.announcement_manager
            .handle_announcement_response(response)
            .await
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
        oversized_enabled: bool,
        oversized_receiver: Arc<RwLock<LruCache<String, OversizedTransferReceiver>>>,
        transfer_policy: TransferPolicy,
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
                // CEP-22 (OD-4): snapshot the flag BEFORE the learning gate mutates
                // it — the very `start` frame carries the client's support tag, so
                // without this snapshot the first transfer would never get an `accept`.
                let client_already_supported = session.supports_oversized_transfer;
                // CEP-22: only learn oversized support if it is enabled on this server.
                session.supports_oversized_transfer |=
                    oversized_enabled && discovered.supports_oversized_transfer;

                // CEP-22: intercept oversized-transfer frames before request
                // correlation/dispatch. A disabled server forwards raw progress
                // notifications as before (OD-6).
                if oversized_enabled {
                    if let JsonRpcMessage::Notification(ref n) = mcp_msg {
                        if OversizedTransferReceiver::is_oversized_frame(n) {
                            drop(sessions_w);
                            Self::handle_oversized_frame(
                                n,
                                &sender_pubkey,
                                &event_id,
                                is_encrypted,
                                is_gift_wrap,
                                outer_kind,
                                client_already_supported,
                                &oversized_receiver,
                                transfer_policy,
                                &relay_pool,
                                encryption_mode,
                                gift_wrap_mode,
                                &event_routes,
                                &request_wrap_kinds,
                                &tx,
                            )
                            .await;
                            continue;
                        }
                    }
                }

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

    /// CEP-22 server inbound: process one oversized-transfer frame.
    ///
    /// Emits an `accept` on the opening frame when the client's support is not yet
    /// known (OD-4), feeds the frame to this peer's reassembler, and — on the
    /// `end` frame — registers a response route and dispatches the reassembled
    /// request as a synthetic [`IncomingRequest`] (keyed by the end frame's real
    /// carrying event id, collision-free against the reserved sentinels).
    #[allow(clippy::too_many_arguments)]
    async fn handle_oversized_frame(
        frame: &JsonRpcNotification,
        sender_pubkey: &str,
        event_id: &str,
        is_encrypted: bool,
        is_gift_wrap: bool,
        outer_kind: u16,
        client_already_supported: bool,
        oversized_receiver: &Arc<RwLock<LruCache<String, OversizedTransferReceiver>>>,
        transfer_policy: TransferPolicy,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        event_routes: &ServerEventRouteStore,
        request_wrap_kinds: &Arc<RwLock<HashMap<String, Option<u16>>>>,
        tx: &tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
    ) {
        // The outer progressToken keys the transfer (needed for accept + route).
        let token = frame
            .params
            .as_ref()
            .and_then(|p| p.get("progressToken"))
            .and_then(|t| t.as_str())
            .map(String::from);

        // 1. Emit `accept` on the opening frame if support is not yet known.
        let is_start = frame
            .params
            .as_ref()
            .and_then(|p| p.get("cvm"))
            .and_then(OversizedFrame::from_cvm_value)
            .is_some_and(|f| matches!(f, OversizedFrame::Start { .. }));
        let issued_accept = is_start && !client_already_supported && token.is_some();
        if issued_accept {
            if let Some(ref token) = token {
                Self::emit_accept_frame(
                    token,
                    sender_pubkey,
                    event_id,
                    is_encrypted,
                    is_gift_wrap,
                    outer_kind,
                    relay_pool,
                    encryption_mode,
                    gift_wrap_mode,
                )
                .await;
            }
        }

        // 2. Feed the frame to this peer's reassembler (process_frame is sync; the
        // write guard is held only across the sync call, never an await). When we
        // issued an `accept`, the sender reserved progress slot 2 and its chunks
        // begin at slot 3 — but we never *receive* an `accept`, so feed a synthetic
        // one into our own receiver to align chunk-slot tracking with the handshake
        // layout (otherwise the frontier sticks at slot 2 and chunks pile up as
        // out-of-order until the gap exceeds the window).
        let outcome = {
            let mut store = oversized_receiver.write().await;
            if !store.contains(sender_pubkey) {
                store.put(
                    sender_pubkey.to_string(),
                    OversizedTransferReceiver::with_policy(transfer_policy),
                );
            }
            let receiver = store.get_mut(sender_pubkey).unwrap();
            let outcome = receiver.process_frame(frame);
            if issued_accept && matches!(outcome, Ok(None)) {
                if let Some(ref token) = token {
                    if let Ok(accept) = OversizedFrame::Accept.into_progress_notification(
                        token,
                        ACCEPT_PROGRESS,
                        None,
                    ) {
                        let _ = receiver.process_frame(&accept);
                    }
                }
            }
            outcome
        };

        match outcome {
            // start/accept/chunk consumed — nothing to dispatch yet.
            Ok(None) => {}
            // The `end` frame: reassembled request ready to dispatch.
            Ok(Some(message)) => {
                let original_id = message.id().cloned().unwrap_or(serde_json::Value::Null);
                // Mirror the incoming wrap kind for the eventual response (CEP-19).
                {
                    let mut kinds_w = request_wrap_kinds.write().await;
                    kinds_w.insert(
                        event_id.to_string(),
                        if is_gift_wrap { Some(outer_kind) } else { None },
                    );
                }
                event_routes
                    .register(
                        event_id.to_string(),
                        sender_pubkey.to_string(),
                        original_id,
                        token,
                    )
                    .await;
                let _ = tx.send(IncomingRequest {
                    message,
                    client_pubkey: sender_pubkey.to_string(),
                    event_id: event_id.to_string(),
                    is_encrypted,
                });
            }
            // D11: clean up locally, let the peer's own timeout fire.
            Err(error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    error = %error,
                    sender_pubkey = %sender_pubkey,
                    "Oversized transfer frame rejected; cleaning up locally"
                );
            }
        }
    }

    /// CEP-22: publish a single `accept` frame back to `sender_pubkey`, e-tagged to
    /// the `start` frame's carrying event. Best-effort — failures are logged only
    /// (the sender falls back to its own accept timeout).
    #[allow(clippy::too_many_arguments)]
    async fn emit_accept_frame(
        token: &str,
        sender_pubkey: &str,
        start_event_id: &str,
        is_encrypted: bool,
        is_gift_wrap: bool,
        outer_kind: u16,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
    ) {
        let client_pk = match PublicKey::from_hex(sender_pubkey) {
            Ok(pk) => pk,
            Err(_) => return,
        };
        let event_id_parsed = EventId::from_hex(start_event_id).unwrap_or(EventId::all_zeros());
        let accept =
            match OversizedFrame::Accept.into_progress_notification(token, ACCEPT_PROGRESS, None) {
                Ok(n) => JsonRpcMessage::Notification(n),
                Err(error) => {
                    tracing::error!(
                        target: LOG_TARGET,
                        error = %error,
                        "Failed to build oversized-transfer accept frame"
                    );
                    return;
                }
            };
        let tags = BaseTransport::create_response_tags(&client_pk, &event_id_parsed);
        let base = BaseTransport {
            relay_pool: Arc::clone(relay_pool),
            encryption_mode,
            is_connected: true,
        };
        if let Err(error) = base
            .send_mcp_message(
                &accept,
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
                error = %error,
                sender_pubkey = %sender_pubkey,
                "Failed to send oversized-transfer accept frame"
            );
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
        assert!(config.relay_list_urls.is_none());
        assert!(config.bootstrap_relay_urls.is_none());
        assert!(config.publish_relay_list);
        assert!(config.profile_metadata.is_none());
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
            internal_common_tags: vec![],
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
            internal_common_tags: vec![],
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
            internal_common_tags: vec![],
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
            internal_common_tags: vec![],
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
            internal_common_tags: vec![],
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

    // ── CEP-22 oversized transfer capability advertisement ──────

    fn first_tag_values(tags: &[Tag]) -> Vec<String> {
        tags.iter().map(|t| t.clone().to_vec()[0].clone()).collect()
    }

    async fn make_server_with_oversized(enabled: bool) -> NostrServerTransport {
        let config = NostrServerTransportConfig {
            oversized_transfer: OversizedTransferConfig::default().with_enabled(enabled),
            ..Default::default()
        };
        let pool: Arc<dyn RelayPoolTrait> = Arc::new(crate::relay::mock::MockRelayPool::new());
        NostrServerTransport::with_relay_pool(config, pool)
            .await
            .expect("server transport construction")
    }

    #[test]
    fn test_oversized_disabled_by_default() {
        let config = NostrServerTransportConfig::default();
        assert!(!config.oversized_transfer.enabled);
    }

    #[test]
    fn test_oversized_support_tags_helper() {
        let mut config = NostrServerTransportConfig::default();
        assert!(oversized_support_tags(&config).is_empty());
        config.oversized_transfer.enabled = true;
        let names = first_tag_values(&oversized_support_tags(&config));
        assert_eq!(names, vec!["support_oversized_transfer"]);
    }

    #[test]
    fn test_oversized_builders() {
        let config = NostrServerTransportConfig::default().with_oversized_enabled(true);
        assert!(config.oversized_transfer.enabled);
        let config = NostrServerTransportConfig::default()
            .with_oversized_transfer(OversizedTransferConfig::enabled().with_threshold(123));
        assert!(config.oversized_transfer.enabled);
        assert_eq!(config.oversized_transfer.threshold, 123);
    }

    #[tokio::test]
    async fn test_announcement_includes_oversized_tag_when_enabled() {
        let server = make_server_with_oversized(true).await;
        let names = first_tag_values(&server.announcement_manager.get_common_tags());
        assert!(
            names.contains(&"support_oversized_transfer".to_string()),
            "announcement common tags must advertise oversized support when enabled"
        );
    }

    #[tokio::test]
    async fn test_announcement_omits_oversized_tag_when_disabled() {
        let server = make_server_with_oversized(false).await;
        let names = first_tag_values(&server.announcement_manager.get_common_tags());
        assert!(
            !names.contains(&"support_oversized_transfer".to_string()),
            "announcement must not advertise oversized support when disabled"
        );
    }

    #[tokio::test]
    async fn test_first_response_snapshot_includes_oversized_tag_when_enabled() {
        let server = make_server_with_oversized(true).await;
        let snapshot = server.announcement_manager.common_tags_snapshot();
        let mut tags = Vec::new();
        snapshot.append_common_response_tags(&mut tags);
        let names = first_tag_values(&tags);
        assert!(
            names.contains(&"support_oversized_transfer".to_string()),
            "first-response replay must carry the oversized tag when enabled"
        );
    }

    #[tokio::test]
    async fn test_first_response_snapshot_omits_oversized_tag_when_disabled() {
        let server = make_server_with_oversized(false).await;
        let snapshot = server.announcement_manager.common_tags_snapshot();
        let mut tags = Vec::new();
        snapshot.append_common_response_tags(&mut tags);
        let names = first_tag_values(&tags);
        assert!(!names.contains(&"support_oversized_transfer".to_string()));
    }

    #[test]
    fn test_server_learns_client_oversized_only_when_enabled() {
        // Unit-level check that `learn_peer_capabilities` parses the client tag and
        // the `enabled && supports` truth table holds. The production gate in
        // `event_loop` is exercised end-to-end by the integration tests
        // `server_gate_allows_oversized_when_enabled` /
        // `server_gate_blocks_oversized_when_disabled` in tests/transport_integration.rs.
        let oversized_tag = Tag::custom(
            TagKind::Custom(tags::SUPPORT_OVERSIZED_TRANSFER.into()),
            Vec::<String>::new(),
        );
        let discovered = learn_peer_capabilities(&[oversized_tag]);
        assert!(discovered.supports_oversized_transfer);

        // Disabled server: client flag must be ignored.
        let mut session = ClientSession::new(false);
        let oversized_enabled = false;
        session.supports_oversized_transfer |=
            oversized_enabled && discovered.supports_oversized_transfer;
        assert!(!session.supports_oversized_transfer);

        // Enabled server: client flag is learned.
        let oversized_enabled = true;
        session.supports_oversized_transfer |=
            oversized_enabled && discovered.supports_oversized_transfer;
        assert!(session.supports_oversized_transfer);
    }
}
