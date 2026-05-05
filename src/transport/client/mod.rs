//! Client-side Nostr transport for ContextVM.
//!
//! Connects to a remote MCP server over Nostr. Sends JSON-RPC requests as
//! kind 25910 events, correlates responses via `e` tag.

pub mod correlation_store;

pub use correlation_store::ClientCorrelationStore;

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lru::LruCache;
use nostr_sdk::prelude::*;
use tokio_util::sync::CancellationToken;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::serializers;
use crate::core::types::*;
use crate::core::validation;
use crate::encryption;
use crate::relay::{RelayPool, RelayPoolTrait};
use crate::transport::base::BaseTransport;
use crate::transport::discovery_tags::{parse_discovered_peer_capabilities, PeerCapabilities};

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
    /// Correlation-retention TTL for pending client requests (default: 30s).
    ///
    /// Stale pending entries older than this are swept from the correlation store.
    /// This prevents leaks -- rmcp owns actual request timeout and cancellation.
    /// Keep this value above your rmcp request timeout to avoid premature cleanup.
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
    pending_requests: ClientCorrelationStore,
    /// CEP-35: one-shot flag for client discovery tag emission.
    has_sent_discovery_tags: AtomicBool,
    /// CEP-35: learned server capabilities from inbound discovery tags.
    discovered_server_capabilities: Arc<Mutex<PeerCapabilities>>,
    /// CEP-35: first inbound event carrying discovery tags (session baseline).
    server_initialize_event: Arc<Mutex<Option<Event>>>,
    /// Learned support for server-side ephemeral gift wraps.
    server_supports_ephemeral: Arc<AtomicBool>,
    /// Outer gift-wrap event IDs successfully decrypted and verified (inner `verify()`).
    /// Duplicate outer ids are skipped before decrypt; ids are inserted only after success
    /// so failed decrypt/verify can be retried on redelivery.
    seen_gift_wrap_ids: Arc<Mutex<LruCache<EventId, ()>>>,
    /// Channel for receiving processed MCP messages from the event loop.
    message_tx: Option<tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>>,
    message_rx: Option<tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>>,
    /// Token used to cancel the spawned event loop on close().
    cancellation_token: CancellationToken,
    /// Handle for the spawned event loop task.
    event_loop_handle: Option<tokio::task::JoinHandle<()>>,
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

        let relay_pool: Arc<dyn RelayPoolTrait> =
            Arc::new(RelayPool::new(signer).await.map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    "Failed to initialize relay pool for client transport"
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
            pending_requests: ClientCorrelationStore::new(),
            has_sent_discovery_tags: AtomicBool::new(false),
            discovered_server_capabilities: Arc::new(Mutex::new(PeerCapabilities::default())),
            server_initialize_event: Arc::new(Mutex::new(None)),
            server_supports_ephemeral: Arc::new(AtomicBool::new(false)),
            seen_gift_wrap_ids,
            message_tx: Some(tx),
            message_rx: Some(rx),
            cancellation_token: CancellationToken::new(),
            event_loop_handle: None,
        })
    }

    /// Like [`new`](Self::new) but accepts an existing relay pool.
    pub async fn with_relay_pool(
        config: NostrClientTransportConfig,
        relay_pool: Arc<dyn RelayPoolTrait>,
    ) -> Result<Self> {
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

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let seen_gift_wrap_ids = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"),
        )));

        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.relay_urls.len(),
            stateless = config.is_stateless,
            encryption_mode = ?config.encryption_mode,
            "Created client transport (with_relay_pool)"
        );
        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            config,
            server_pubkey,
            pending_requests: ClientCorrelationStore::new(),
            has_sent_discovery_tags: AtomicBool::new(false),
            discovered_server_capabilities: Arc::new(Mutex::new(PeerCapabilities::default())),
            server_initialize_event: Arc::new(Mutex::new(None)),
            server_supports_ephemeral: Arc::new(AtomicBool::new(false)),
            seen_gift_wrap_ids,
            message_tx: Some(tx),
            message_rx: Some(rx),
            cancellation_token: CancellationToken::new(),
            event_loop_handle: None,
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

        // Spawn event loop with cancellation support
        let relay_pool = Arc::clone(&self.base.relay_pool);
        let pending = self.pending_requests.clone();
        let server_pubkey = self.server_pubkey;
        let tx = self
            .message_tx
            .as_ref()
            .expect("message_tx must exist before start()")
            .clone();
        let encryption_mode = self.config.encryption_mode;
        let gift_wrap_mode = self.config.gift_wrap_mode;
        let discovered_caps = self.discovered_server_capabilities.clone();
        let init_event = self.server_initialize_event.clone();
        let server_supports_ephemeral = self.server_supports_ephemeral.clone();
        let seen_gift_wrap_ids = self.seen_gift_wrap_ids.clone();
        let timeout = self.config.timeout;
        let token = self.cancellation_token.child_token();

        self.event_loop_handle = Some(tokio::spawn(async move {
            Self::event_loop(
                relay_pool,
                pending,
                server_pubkey,
                tx,
                encryption_mode,
                gift_wrap_mode,
                discovered_caps,
                init_event,
                server_supports_ephemeral,
                seen_gift_wrap_ids,
                timeout,
                token,
            )
            .await;
        }));

        tracing::info!(
            target: LOG_TARGET,
            relay_count = self.config.relay_urls.len(),
            "Client transport event loop spawned"
        );
        Ok(())
    }

    /// Close the transport — cancels the event loop and disconnects from relays.
    pub async fn close(&mut self) -> Result<()> {
        self.cancellation_token.cancel();
        if let Some(handle) = self.event_loop_handle.take() {
            let _ = handle.await;
        }
        self.message_tx.take();
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

        let is_request = message.is_request();
        let base_tags = BaseTransport::create_recipient_tags(&self.server_pubkey);
        let discovery_tags = if is_request {
            self.get_pending_client_discovery_tags()
        } else {
            vec![]
        };
        let tags = BaseTransport::compose_outbound_tags(&base_tags, &discovery_tags, &[]);

        let (event_id, publishable_event) = self
            .base
            .prepare_mcp_message(
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
                    "Failed to prepare client message"
                );
                error
            })?;

        if let JsonRpcMessage::Request(ref req) = message {
            let is_initialize = req.method == INITIALIZE_METHOD;
            self.pending_requests
                .register(event_id.to_hex(), req.id.clone(), is_initialize)
                .await;
        }

        if let Err(error) = self.base.relay_pool.publish_event(&publishable_event).await {
            self.pending_requests.remove(&event_id.to_hex()).await;
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                server_pubkey = %self.server_pubkey.to_hex(),
                method = ?message.method(),
                "Failed to publish client message"
            );
            return Err(error);
        }

        // Flip one-shot flag only after successful publish
        if is_request && !discovery_tags.is_empty() {
            self.has_sent_discovery_tags.store(true, Ordering::Relaxed);
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
        if let Some(ref tx) = self.message_tx {
            let _ = tx.send(response);
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn event_loop(
        relay_pool: Arc<dyn RelayPoolTrait>,
        pending: ClientCorrelationStore,
        server_pubkey: PublicKey,
        tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        discovered_caps: Arc<Mutex<PeerCapabilities>>,
        init_event: Arc<Mutex<Option<Event>>>,
        server_supports_ephemeral: Arc<AtomicBool>,
        seen_gift_wrap_ids: Arc<Mutex<LruCache<EventId, ()>>>,
        timeout: Duration,
        cancel: CancellationToken,
    ) {
        let mut notifications = relay_pool.notifications();
        // Sweep interval: half the timeout, clamped to [1s, 30s].
        let sweep_interval = (timeout / 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
        let mut sweep_timer =
            tokio::time::interval_at(tokio::time::Instant::now() + sweep_interval, sweep_interval);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(
                        target: LOG_TARGET,
                        "Client event loop cancelled"
                    );
                    break;
                }
                result = notifications.recv() => {
                    let notification = match result {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    Self::handle_notification(
                        &notification,
                        &pending,
                        server_pubkey,
                        &tx,
                        encryption_mode,
                        gift_wrap_mode,
                        &discovered_caps,
                        &init_event,
                        &server_supports_ephemeral,
                        &seen_gift_wrap_ids,
                        &relay_pool,
                    )
                    .await;
                }
                _ = sweep_timer.tick() => {
                    let swept = pending.sweep_expired(timeout).await;
                    if swept > 0 {
                        tracing::warn!(
                            target: LOG_TARGET,
                            swept,
                            timeout_ms = timeout.as_millis() as u64,
                            "Swept stale pending requests (rmcp handles timeout errors)"
                        );
                    }
                }
            }
        }
    }

    // ── CEP-35 discovery tag helpers ──────────────────────────────

    /// Constructs client capability tags based on config.
    fn get_client_capability_tags(&self) -> Vec<Tag> {
        let mut tags = Vec::new();
        if self.config.encryption_mode != EncryptionMode::Disabled {
            tags.push(Tag::custom(
                TagKind::Custom(tags::SUPPORT_ENCRYPTION.into()),
                Vec::<String>::new(),
            ));
            if self.config.gift_wrap_mode != GiftWrapMode::Persistent {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
                    Vec::<String>::new(),
                ));
            }
        }
        tags
    }

    /// One-shot: returns capability tags if not yet sent, empty otherwise.
    fn get_pending_client_discovery_tags(&self) -> Vec<Tag> {
        if self.has_sent_discovery_tags.load(Ordering::Relaxed) {
            vec![]
        } else {
            self.get_client_capability_tags()
        }
    }

    /// Parses inbound event tags and updates learned server capabilities.
    fn learn_server_discovery(
        discovered_caps: &Mutex<PeerCapabilities>,
        init_event: &Mutex<Option<Event>>,
        event: &Event,
    ) {
        let tag_vec: Vec<Tag> = event.tags.clone().to_vec();
        let discovered = parse_discovered_peer_capabilities(&tag_vec);
        if discovered.discovery_tags.is_empty() {
            return;
        }

        {
            let mut caps = match discovered_caps.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            caps.supports_encryption |= discovered.capabilities.supports_encryption;
            caps.supports_ephemeral_encryption |=
                discovered.capabilities.supports_ephemeral_encryption;
            caps.supports_oversized_transfer |= discovered.capabilities.supports_oversized_transfer;
        }

        let mut stored = match init_event.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if stored.is_none() {
            *stored = Some(event.clone());
        }
        // Note: TS SDK has an upgrade path where a later event with an InitializeResult
        // replaces a non-initialize baseline. Not implemented here -- edge case only
        // relevant if the first server message with discovery tags is a notification.
    }

    /// Returns a clone of the first inbound event that carried server discovery tags.
    pub fn get_server_initialize_event(&self) -> Option<Event> {
        let guard = match self.server_initialize_event.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.clone()
    }

    /// Returns a snapshot of the learned server capabilities from discovery tags.
    pub fn discovered_server_capabilities(&self) -> PeerCapabilities {
        let guard = match self.discovered_server_capabilities.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_notification(
        notification: &RelayPoolNotification,
        pending: &ClientCorrelationStore,
        server_pubkey: PublicKey,
        tx: &tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        discovered_caps: &Arc<Mutex<PeerCapabilities>>,
        init_event: &Arc<Mutex<Option<Event>>>,
        server_supports_ephemeral: &Arc<AtomicBool>,
        seen_gift_wrap_ids: &Arc<Mutex<LruCache<EventId, ()>>>,
        relay_pool: &Arc<dyn RelayPoolTrait>,
    ) {
        let event = match notification {
            RelayPoolNotification::Event { event, .. } => event,
            _ => return,
        };

        let is_gift_wrap = is_gift_wrap_kind(&event.kind);
        let outer_kind = event.kind.as_u16();

        // Enforce encryption mode before decrypt/parse.
        if violates_encryption_policy(&event.kind, &encryption_mode) {
            if is_gift_wrap {
                tracing::warn!(
                    target: LOG_TARGET,
                    event_id = %event.id.to_hex(),
                    event_kind = outer_kind,
                    configured_mode = ?gift_wrap_mode,
                    "Skipping encrypted response because client encryption is disabled"
                );
            } else {
                tracing::warn!(
                    target: LOG_TARGET,
                    event_id = %event.id.to_hex(),
                    "Skipping plaintext response because client encryption is required"
                );
            }
            return;
        }

        // Enforce CEP-19 gift-wrap-mode policy.
        if is_gift_wrap && !gift_wrap_mode.allows_kind(outer_kind) {
            tracing::warn!(
                target: LOG_TARGET,
                event_id = %event.id.to_hex(),
                event_kind = outer_kind,
                configured_mode = ?gift_wrap_mode,
                "Skipping gift wrap due to CEP-19 policy"
            );
            return;
        }

        // Handle gift-wrapped events
        let (actual_event_content, actual_pubkey, e_tag, verified_tags, source_event) =
            if is_gift_wrap {
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
                        return;
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
                        return;
                    }
                };
                match encryption::decrypt_gift_wrap_single_layer(&signer, event).await {
                    Ok(decrypted_json) => match serde_json::from_str::<Event>(&decrypted_json) {
                        Ok(inner) => {
                            if let Err(e) = inner.verify() {
                                tracing::warn!("Inner event signature verification failed: {e}");
                                return;
                            }
                            {
                                let mut guard = match seen_gift_wrap_ids.lock() {
                                    Ok(g) => g,
                                    Err(poisoned) => poisoned.into_inner(),
                                };
                                guard.put(event.id, ());
                            }
                            let e_tag = serializers::get_tag_value(&inner.tags, "e");
                            let inner_clone = inner.clone();
                            (inner.content, inner.pubkey, e_tag, inner.tags, inner_clone)
                        }
                        Err(error) => {
                            tracing::error!(
                                target: LOG_TARGET,
                                error = %error,
                                "Failed to parse inner event"
                            );
                            return;
                        }
                    },
                    Err(error) => {
                        tracing::error!(
                            target: LOG_TARGET,
                            error = %error,
                            "Failed to decrypt gift wrap"
                        );
                        return;
                    }
                }
            } else {
                let e_tag = serializers::get_tag_value(&event.tags, "e");
                let event_clone: Event = (**event).clone();
                (
                    event.content.clone(),
                    event.pubkey,
                    e_tag,
                    event.tags.clone(),
                    event_clone,
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
            return;
        }

        // CEP-35: learn server capabilities from discovery tags
        Self::learn_server_discovery(discovered_caps, init_event, &source_event);

        // CEP-19: learn ephemeral support from server
        if Self::should_learn_ephemeral_support(
            actual_pubkey,
            server_pubkey,
            if is_gift_wrap { Some(outer_kind) } else { None },
            &verified_tags,
        ) {
            server_supports_ephemeral.store(true, Ordering::Relaxed);
        }

        // Correlate response
        if let Some(ref correlated_id) = e_tag {
            let is_pending = pending.contains(correlated_id.as_str()).await;
            if !is_pending {
                tracing::warn!(
                    target: LOG_TARGET,
                    correlated_event_id = %correlated_id,
                    "Response for unknown request"
                );
                return;
            }
        }

        // Parse MCP message
        if let Some(mcp_msg) = validation::validate_and_parse(&actual_event_content) {
            // Drop uncorrelated responses and server-to-client requests (matches TS SDK).
            match &mcp_msg {
                JsonRpcMessage::Response(_) | JsonRpcMessage::ErrorResponse(_)
                    if e_tag.is_none() =>
                {
                    tracing::warn!(
                        target: LOG_TARGET,
                        "Dropping response/error without correlation `e` tag"
                    );
                    return;
                }
                JsonRpcMessage::Request(_) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        method = ?mcp_msg.method(),
                        "Dropping server-to-client request (invalid in MCP)"
                    );
                    return;
                }
                _ => {}
            }

            // Clean up pending request
            if let Some(ref correlated_id) = e_tag {
                pending.remove(correlated_id.as_str()).await;
            }
            let _ = tx.send(mcp_msg);
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

    /// Returns whether the client has learned ephemeral gift-wrap support from the server.
    pub fn server_supports_ephemeral_encryption(&self) -> bool {
        self.server_supports_ephemeral.load(Ordering::Relaxed)
    }
}

#[inline]
fn is_gift_wrap_kind(kind: &Kind) -> bool {
    *kind == Kind::Custom(GIFT_WRAP_KIND) || *kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)
}

/// Returns `true` when the inbound event kind violates the configured encryption
/// policy and must be dropped before any further processing.
#[inline]
fn violates_encryption_policy(kind: &Kind, mode: &EncryptionMode) -> bool {
    let is_gift_wrap = is_gift_wrap_kind(kind);
    (is_gift_wrap && *mode == EncryptionMode::Disabled)
        || (!is_gift_wrap && *mode == EncryptionMode::Required)
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
    fn test_custom_timeout_config() {
        let config = NostrClientTransportConfig {
            timeout: Duration::from_secs(60),
            ..Default::default()
        };
        assert_eq!(config.timeout, Duration::from_secs(60));
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
    fn test_has_support_ephemeral_tag_absent() {
        let tags = Tags::from_list(vec![Tag::custom(
            TagKind::Custom(crate::core::constants::tags::SUPPORT_ENCRYPTION.into()),
            Vec::<String>::new(),
        )]);
        assert!(!NostrClientTransport::has_support_ephemeral_tag(&tags));
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
    fn test_should_learn_from_ephemeral_kind_even_without_tag() {
        let server_keys = Keys::generate();
        let empty_tags = Tags::from_list(vec![]);

        assert!(NostrClientTransport::should_learn_ephemeral_support(
            server_keys.public_key(),
            server_keys.public_key(),
            Some(EPHEMERAL_GIFT_WRAP_KIND),
            &empty_tags,
        ));
    }

    #[test]
    fn test_should_learn_from_tag_without_ephemeral_kind() {
        let server_keys = Keys::generate();
        let tags = Tags::from_list(vec![Tag::custom(
            TagKind::Custom(crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
            Vec::<String>::new(),
        )]);

        assert!(NostrClientTransport::should_learn_ephemeral_support(
            server_keys.public_key(),
            server_keys.public_key(),
            Some(GIFT_WRAP_KIND), // persistent kind, but tag present
            &tags,
        ));
    }

    #[test]
    fn test_stateless_emulated_initialize_response_shape() {
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

    #[test]
    fn test_gift_wrap_kind_detection() {
        assert!(is_gift_wrap_kind(&Kind::Custom(GIFT_WRAP_KIND)));
        assert!(is_gift_wrap_kind(&Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)));
        assert!(!is_gift_wrap_kind(&Kind::Custom(CTXVM_MESSAGES_KIND)));
    }

    #[test]
    fn test_required_mode_drops_plaintext() {
        let plaintext_kind = Kind::Custom(CTXVM_MESSAGES_KIND);
        assert!(
            violates_encryption_policy(&plaintext_kind, &EncryptionMode::Required),
            "Required mode must reject plaintext (non-gift-wrap) events"
        );
    }

    #[test]
    fn test_disabled_mode_drops_encrypted() {
        assert!(
            violates_encryption_policy(&Kind::Custom(GIFT_WRAP_KIND), &EncryptionMode::Disabled),
            "Disabled mode must reject gift-wrap events"
        );
        assert!(
            violates_encryption_policy(
                &Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND),
                &EncryptionMode::Disabled
            ),
            "Disabled mode must reject ephemeral gift-wrap events"
        );
    }

    #[test]
    fn test_optional_mode_accepts_all() {
        let plaintext = Kind::Custom(CTXVM_MESSAGES_KIND);
        let gift_wrap = Kind::Custom(GIFT_WRAP_KIND);
        let ephemeral = Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND);
        assert!(!violates_encryption_policy(
            &plaintext,
            &EncryptionMode::Optional
        ));
        assert!(!violates_encryption_policy(
            &gift_wrap,
            &EncryptionMode::Optional
        ));
        assert!(!violates_encryption_policy(
            &ephemeral,
            &EncryptionMode::Optional
        ));
    }

    #[test]
    fn test_required_mode_accepts_encrypted() {
        assert!(
            !violates_encryption_policy(&Kind::Custom(GIFT_WRAP_KIND), &EncryptionMode::Required),
            "Required mode must accept gift-wrap events"
        );
        assert!(
            !violates_encryption_policy(
                &Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND),
                &EncryptionMode::Required
            ),
            "Required mode must accept ephemeral gift-wrap events"
        );
    }

    #[test]
    fn test_disabled_mode_accepts_plaintext() {
        let plaintext = Kind::Custom(CTXVM_MESSAGES_KIND);
        assert!(
            !violates_encryption_policy(&plaintext, &EncryptionMode::Disabled),
            "Disabled mode must accept plaintext events"
        );
    }

    // ── CEP-35 client discovery tag emission ────────────────────

    fn make_transport_for_tags(
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
    ) -> NostrClientTransport {
        let keys = Keys::generate();
        NostrClientTransport {
            base: BaseTransport {
                relay_pool: Arc::new(crate::relay::mock::MockRelayPool::new()),
                encryption_mode,
                is_connected: false,
            },
            config: NostrClientTransportConfig {
                encryption_mode,
                gift_wrap_mode,
                server_pubkey: Keys::generate().public_key().to_hex(),
                ..Default::default()
            },
            server_pubkey: keys.public_key(),
            pending_requests: ClientCorrelationStore::new(),
            has_sent_discovery_tags: AtomicBool::new(false),
            discovered_server_capabilities: Arc::new(Mutex::new(PeerCapabilities::default())),
            server_initialize_event: Arc::new(Mutex::new(None)),
            server_supports_ephemeral: Arc::new(AtomicBool::new(false)),
            seen_gift_wrap_ids: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(10).unwrap()))),
            message_tx: Some(tokio::sync::mpsc::unbounded_channel().0),
            message_rx: None,
            cancellation_token: CancellationToken::new(),
            event_loop_handle: None,
        }
    }

    fn make_tag(parts: &[&str]) -> Tag {
        let kind = TagKind::Custom(parts[0].into());
        let values: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        Tag::custom(kind, values)
    }

    fn tag_names(tags: &[Tag]) -> Vec<String> {
        tags.iter().map(|t| t.clone().to_vec()[0].clone()).collect()
    }

    #[test]
    fn client_capability_tags_encryption_optional() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        let tags = t.get_client_capability_tags();
        let names = tag_names(&tags);
        assert_eq!(
            names,
            vec!["support_encryption", "support_encryption_ephemeral"]
        );
    }

    #[test]
    fn client_capability_tags_encryption_disabled() {
        let t = make_transport_for_tags(EncryptionMode::Disabled, GiftWrapMode::Optional);
        let tags = t.get_client_capability_tags();
        assert!(tags.is_empty());
    }

    #[test]
    fn client_capability_tags_persistent_gift_wrap() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Persistent);
        let tags = t.get_client_capability_tags();
        let names = tag_names(&tags);
        assert_eq!(names, vec!["support_encryption"]);
    }

    #[test]
    fn client_discovery_tags_sent_once() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        let first = t.get_pending_client_discovery_tags();
        assert!(!first.is_empty());

        t.has_sent_discovery_tags.store(true, Ordering::Relaxed);
        let second = t.get_pending_client_discovery_tags();
        assert!(second.is_empty());
    }

    // ── CEP-35 client capability learning ───────────────────────

    fn make_event_with_tags(tag_parts: &[&[&str]]) -> Event {
        let keys = Keys::generate();
        let tags: Vec<Tag> = tag_parts.iter().map(|p| make_tag(p)).collect();
        let builder = EventBuilder::new(Kind::Custom(CTXVM_MESSAGES_KIND), "{}").tags(tags);
        let unsigned = builder.build(keys.public_key());
        unsigned.sign_with_keys(&keys).unwrap()
    }

    #[test]
    fn client_learn_server_discovery_sets_baseline() {
        let caps = Mutex::new(PeerCapabilities::default());
        let init = Mutex::new(None);
        let event = make_event_with_tags(&[&["support_encryption"], &["name", "TestServer"]]);

        NostrClientTransport::learn_server_discovery(&caps, &init, &event);

        let c = caps.lock().unwrap();
        assert!(c.supports_encryption);
        assert!(!c.supports_ephemeral_encryption);

        let stored = init.lock().unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.as_ref().unwrap().id, event.id);
    }

    #[test]
    fn client_learn_server_discovery_or_assigns() {
        let caps = Mutex::new(PeerCapabilities::default());
        let init = Mutex::new(None);

        let event1 = make_event_with_tags(&[&["support_encryption"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &event1);

        // Second event with different caps does NOT downgrade
        let event2 = make_event_with_tags(&[&["support_encryption_ephemeral"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &event2);

        let c = caps.lock().unwrap();
        assert!(c.supports_encryption, "must not downgrade");
        assert!(c.supports_ephemeral_encryption, "must learn new cap");
    }

    #[test]
    fn client_baseline_not_replaced_on_later_events() {
        let caps = Mutex::new(PeerCapabilities::default());
        let init = Mutex::new(None);

        let event1 = make_event_with_tags(&[&["support_encryption"], &["name", "First"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &event1);
        let first_id = event1.id;

        let event2 =
            make_event_with_tags(&[&["support_encryption_ephemeral"], &["name", "Second"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &event2);

        let stored = init.lock().unwrap();
        assert_eq!(
            stored.as_ref().unwrap().id,
            first_id,
            "baseline must not be replaced"
        );
    }
}
