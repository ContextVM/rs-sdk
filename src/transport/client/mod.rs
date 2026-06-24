//! Client-side Nostr transport for ContextVM.
//!
//! Connects to a remote MCP server over Nostr. Sends JSON-RPC requests as
//! kind 25910 events, correlates responses via `e` tag.

pub mod correlation_store;
pub mod relay_resolution;
pub mod server_identity;
pub mod server_relay_discovery;

pub use correlation_store::ClientCorrelationStore;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lru::LruCache;
use nostr_sdk::prelude::*;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::constants::*;
use crate::core::error::Result;
use crate::core::serializers;
use crate::core::types::*;
use crate::core::validation;
use crate::encryption;
use crate::relay::{RelayPool, RelayPoolTrait};
use crate::transport::base::BaseTransport;
use crate::transport::discovery_tags::{parse_discovered_peer_capabilities, PeerCapabilities};
use crate::transport::open_stream::OpenStreamConfig;
use crate::transport::oversized_transfer::{
    build_oversized_frames, progress_token_string, resolve_safe_chunk_size,
    send_oversized_transfer, OversizedFrame, OversizedSenderOptions, OversizedTransferConfig,
    OversizedTransferReceiver, NOTIFICATIONS_PROGRESS_METHOD,
};

const LOG_TARGET: &str = "contextvm_sdk::transport::client";

/// Configuration for the client transport.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NostrClientTransportConfig {
    /// Relay URLs to connect to.
    pub relay_urls: Vec<String>,
    /// The server's public key (hex, npub, or nprofile).
    ///
    /// When an nprofile is provided, embedded relay hints are extracted and used
    /// during CEP-17 relay resolution.
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
    /// Relay URLs used for CEP-17 relay-list discovery when operational relays are not configured.
    /// Overrides `DEFAULT_BOOTSTRAP_RELAY_URLS` when provided.
    pub discovery_relay_urls: Option<Vec<String>>,
    /// Non-authoritative operational relays probed in parallel with CEP-17 discovery.
    pub fallback_operational_relay_urls: Option<Vec<String>>,
    /// CEP-22 oversized payload transfer configuration. Enabled by default.
    pub oversized_transfer: OversizedTransferConfig,
    /// CEP-41 open-stream configuration. Disabled by default (opt-in).
    ///
    /// **Data only in PR1** — the event loop does not consult it yet; activation
    /// (capability advertisement, learning, `call_tool_stream`) lands in PR2.
    pub open_stream: OpenStreamConfig,
}

impl Default for NostrClientTransportConfig {
    fn default() -> Self {
        Self {
            relay_urls: vec![],
            server_pubkey: String::new(),
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            is_stateless: false,
            timeout: Duration::from_secs(30),
            discovery_relay_urls: None,
            fallback_operational_relay_urls: None,
            oversized_transfer: OversizedTransferConfig::default(),
            open_stream: OpenStreamConfig::default(),
        }
    }
}

impl NostrClientTransportConfig {
    /// Set the server's public key (hex, npub, or nprofile).
    pub fn with_server_pubkey(mut self, pubkey: impl Into<String>) -> Self {
        self.server_pubkey = pubkey.into();
        self
    }
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
    /// Enable or disable stateless mode.
    pub fn with_stateless(mut self, stateless: bool) -> Self {
        self.is_stateless = stateless;
        self
    }
    /// Set the relay URLs to connect to.
    pub fn with_relay_urls(mut self, urls: Vec<String>) -> Self {
        self.relay_urls = urls;
        self
    }
    /// Set the correlation-retention TTL.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    /// Set relay URLs for CEP-17 relay-list discovery.
    pub fn with_discovery_relay_urls(mut self, urls: Vec<String>) -> Self {
        self.discovery_relay_urls = Some(urls);
        self
    }
    /// Set fallback operational relay URLs probed in parallel with discovery.
    pub fn with_fallback_operational_relay_urls(mut self, urls: Vec<String>) -> Self {
        self.fallback_operational_relay_urls = Some(urls);
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
    /// Set the full CEP-41 open-stream configuration.
    ///
    /// Data only in PR1: the event loop does not read this until PR2.
    pub fn with_open_stream(mut self, config: OpenStreamConfig) -> Self {
        self.open_stream = config;
        self
    }
}

/// Client-side Nostr transport for sending MCP requests and receiving responses.
pub struct NostrClientTransport {
    base: BaseTransport,
    config: NostrClientTransportConfig,
    server_pubkey: PublicKey,
    /// Populated from nprofile relay hints; used by relay resolution in `start()` (CEP-17).
    hinted_relay_urls: Vec<String>,
    /// Discovery relay URLs for CEP-17 kind 10002 lookup.
    discovery_relay_urls: Vec<String>,
    /// Fallback operational relay URLs probed in parallel with discovery.
    fallback_operational_relay_urls: Vec<String>,
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
    /// CEP-22: reassembly engine for inbound oversized responses from the server
    /// (single peer). Cleared on [`close`](Self::close).
    oversized_receiver: Arc<Mutex<OversizedTransferReceiver>>,
    /// CEP-22: outstanding `accept` handshake waiters keyed by `progressToken`. A
    /// `send()` awaiting the server's `accept` registers a one-shot here before
    /// publishing `start`; the event loop fires it when the `accept` frame arrives.
    accept_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    /// CEP-22: original `_meta.progressToken` JSON values of sent
    /// oversized-eligible requests, keyed by their stringified form. Frames
    /// stringify tokens on the wire (both SDKs), so the original value —
    /// `Number` for rmcp-issued tokens — survives only here; progress forwarded
    /// to the requester must restore it for rmcp's watcher lookup to match
    /// (`Number(5)` ≠ `String("5")`). LRU-bounded; entries are dropped when
    /// their transfer concludes and cleared on [`close`](Self::close).
    original_progress_tokens: Arc<Mutex<LruCache<String, serde_json::Value>>>,
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
        let (server_pubkey, hinted_relay_urls) =
            server_identity::parse_server_identity(&config.server_pubkey).map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    server_pubkey = %config.server_pubkey,
                    "Invalid server pubkey"
                );
                error
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
        let discovery_relay_urls = config.discovery_relay_urls.clone().unwrap_or_else(|| {
            DEFAULT_BOOTSTRAP_RELAY_URLS
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
        let fallback_operational_relay_urls = config
            .fallback_operational_relay_urls
            .clone()
            .unwrap_or_default();

        let oversized_receiver = Arc::new(Mutex::new(OversizedTransferReceiver::with_policy(
            (&config.oversized_transfer).into(),
        )));
        let accept_waiters = Arc::new(Mutex::new(HashMap::new()));
        let original_progress_tokens = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"),
        )));

        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            oversized_receiver,
            accept_waiters,
            original_progress_tokens,
            config,
            server_pubkey,
            hinted_relay_urls,
            discovery_relay_urls,
            fallback_operational_relay_urls,
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
        let (server_pubkey, hinted_relay_urls) =
            server_identity::parse_server_identity(&config.server_pubkey).map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    server_pubkey = %config.server_pubkey,
                    "Invalid server pubkey"
                );
                error
            })?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let seen_gift_wrap_ids = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"),
        )));

        let discovery_relay_urls = config.discovery_relay_urls.clone().unwrap_or_else(|| {
            DEFAULT_BOOTSTRAP_RELAY_URLS
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
        let fallback_operational_relay_urls = config
            .fallback_operational_relay_urls
            .clone()
            .unwrap_or_default();

        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.relay_urls.len(),
            stateless = config.is_stateless,
            encryption_mode = ?config.encryption_mode,
            "Created client transport (with_relay_pool)"
        );
        let oversized_receiver = Arc::new(Mutex::new(OversizedTransferReceiver::with_policy(
            (&config.oversized_transfer).into(),
        )));
        let accept_waiters = Arc::new(Mutex::new(HashMap::new()));
        let original_progress_tokens = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"),
        )));

        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            oversized_receiver,
            accept_waiters,
            original_progress_tokens,
            config,
            server_pubkey,
            hinted_relay_urls,
            discovery_relay_urls,
            fallback_operational_relay_urls,
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
        let resolved_urls =
            relay_resolution::resolve_operational_relays(relay_resolution::RelayResolutionConfig {
                configured_relay_urls: self.config.relay_urls.clone(),
                hinted_relay_urls: self.hinted_relay_urls.clone(),
                discovery_relay_urls: self.discovery_relay_urls.clone(),
                fallback_operational_relay_urls: self.fallback_operational_relay_urls.clone(),
                server_pubkey: self.server_pubkey,
                signer: self.base.relay_pool.signer().await?,
                timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            })
            .await;

        let connect_urls = if resolved_urls.is_empty() {
            &self.config.relay_urls
        } else {
            &resolved_urls
        };

        self.base.connect(connect_urls).await.map_err(|error| {
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
        let oversized_receiver = self.oversized_receiver.clone();
        let accept_waiters = self.accept_waiters.clone();
        let original_progress_tokens = self.original_progress_tokens.clone();
        let oversized_enabled = self.config.oversized_transfer.enabled;
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
                oversized_receiver,
                accept_waiters,
                original_progress_tokens,
                oversized_enabled,
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
        // CEP-22: release reassembly state and drop any accept waiters so an
        // in-flight `send()` awaiter unblocks (cancelled) instead of hanging to
        // its accept timeout.
        {
            let mut receiver = match self.oversized_receiver.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            receiver.clear();
        }
        {
            let mut waiters = match self.accept_waiters.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            waiters.clear();
        }
        {
            let mut originals = match self.original_progress_tokens.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            originals.clear();
        }
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
        let gift_wrap_kind = self.choose_outbound_gift_wrap_kind();
        let discovery_sent = !discovery_tags.is_empty();

        // CEP-22: only a request carrying a `progressToken` is eligible for oversized
        // fragmentation (the token addresses the frames); extract it once up front.
        // Tokens may be JSON strings or numbers (rmcp issues numbers): the
        // stringified form keys all transport state, and the original value is
        // recorded so progress forwarded to the requester can restore the token's
        // wire type.
        let oversized_token: Option<String> =
            if is_request && self.config.oversized_transfer.enabled {
                let original = match message {
                    JsonRpcMessage::Request(req) => req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken")),
                    _ => None,
                };
                let token = original.and_then(progress_token_string);
                if let (Some(token), Some(original)) = (token.as_deref(), original) {
                    self.record_original_progress_token(token, original);
                }
                token
            } else {
                None
            };

        // CEP-22: fragment when the message would not fit in a single Nostr event.
        // Relay size limits apply to the *published* event, so the decision is made
        // on the published byte size — not the raw payload — which is what actually
        // grows under JSON escaping and gift-wrap encryption (mirrors TS
        // `measurePublishedMcpMessageSize`). The raw serialized length is a cheap
        // lower bound: when it already meets the threshold the message is
        // conclusively oversized and we fragment without building a single event —
        // an escape-heavy payload could otherwise overflow NIP-44's plaintext limit
        // while we measure.
        if let Some(token) = oversized_token.as_deref() {
            let content = serde_json::to_string(message)?;
            let threshold = self.config.oversized_transfer.threshold;
            if content.len() >= threshold {
                return self
                    .send_oversized_request(
                        message,
                        &content,
                        token,
                        base_tags,
                        tags,
                        discovery_sent,
                    )
                    .await;
            }
            // Borderline: a sub-threshold payload can still cross the threshold once
            // signed, JSON-escaped, and (when enabled) gift-wrapped. Build the single
            // event once, measure its real published size, and reuse it if it fits.
            match self
                .base
                .prepare_mcp_message(
                    message,
                    &self.server_pubkey,
                    CTXVM_MESSAGES_KIND,
                    tags.clone(),
                    None,
                    Some(gift_wrap_kind),
                )
                .await
            {
                Ok((event_id, publishable_event)) => {
                    let published_len = serde_json::to_string(&publishable_event)
                        .map(|s| s.len())
                        .unwrap_or(usize::MAX);
                    if published_len > threshold {
                        return self
                            .send_oversized_request(
                                message,
                                &content,
                                token,
                                base_tags,
                                tags,
                                discovery_sent,
                            )
                            .await;
                    }
                    return self
                        .publish_single_event(message, event_id, publishable_event, discovery_sent)
                        .await;
                }
                Err(error) => {
                    // Could not build even one event (e.g. NIP-44 plaintext overflow
                    // from an escape-heavy payload) → it cannot be sent as a single
                    // event; fragment it.
                    tracing::debug!(
                        target: LOG_TARGET,
                        error = %error,
                        "Single-event build failed; sending as oversized transfer"
                    );
                    return self
                        .send_oversized_request(
                            message,
                            &content,
                            token,
                            base_tags,
                            tags,
                            discovery_sent,
                        )
                        .await;
                }
            }
        }

        // Single-event path: not oversized-eligible (notification, feature disabled,
        // or no progressToken).
        let (event_id, publishable_event) = self
            .base
            .prepare_mcp_message(
                message,
                &self.server_pubkey,
                CTXVM_MESSAGES_KIND,
                tags,
                None,
                Some(gift_wrap_kind),
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

        self.publish_single_event(message, event_id, publishable_event, discovery_sent)
            .await
    }

    /// Register (for requests) and publish one prepared MCP event, flipping the
    /// one-shot discovery flag after a successful publish. Shared by the
    /// non-oversized send paths so the event built for the CEP-22 size check is
    /// reused for publishing rather than re-encrypted.
    async fn publish_single_event(
        &self,
        message: &JsonRpcMessage,
        event_id: EventId,
        publishable_event: Event,
        discovery_sent: bool,
    ) -> Result<()> {
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
        if discovery_sent {
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

    /// CEP-22: publish a request as an ordered oversized-transfer sequence.
    ///
    /// Builds `start → chunks… → end` frames, registers an `accept` waiter before
    /// publishing `start` when the server's support is not yet known, drives the
    /// [`send_oversized_transfer`] sequencer, and registers the pending request
    /// against the **end** frame's event id (the value the server correlates its
    /// response to). One-shot discovery tags ride the `start` frame only.
    async fn send_oversized_request(
        &self,
        message: &JsonRpcMessage,
        content: &str,
        token: &str,
        base_tags: Vec<Tag>,
        start_tags: Vec<Tag>,
        discovery_sent: bool,
    ) -> Result<()> {
        // The handshake is required until the server is known to support oversized
        // transfer; once learned, chunks start immediately (no accept slot).
        let needs_accept = !self
            .discovered_server_capabilities()
            .supports_oversized_transfer;

        let gift_wrap_kind = self.choose_outbound_gift_wrap_kind();
        // Effective encryption for these frames (the publish closure passes `None`,
        // letting `should_encrypt` decide from the mode — resolve the same boolean
        // here so the sizing measurement matches the real published frames).
        let is_encrypted = self.base.should_encrypt(CTXVM_MESSAGES_KIND, None);

        // CEP-22: derive a per-chunk payload budget so every published frame stays
        // under the threshold even after the JSON-RPC envelope, signature, and
        // (when encrypted) gift-wrap expansion. Mirrors TS `resolveSafeOversizedChunkSize`.
        // Continuation (chunk) frames carry the bare recipient `p`-tags (`base_tags`),
        // so size against those.
        let chunk_size = resolve_safe_chunk_size(
            self.config.oversized_transfer.chunk_size,
            &self.base,
            &self.server_pubkey,
            &base_tags,
            is_encrypted,
            Kind::Custom(gift_wrap_kind),
            self.config.oversized_transfer.threshold,
        )
        .await?;

        let options = OversizedSenderOptions::new(token)
            .with_chunk_size(chunk_size)
            .with_accept_handshake(needs_accept);
        let frames = build_oversized_frames(content, &options)?;

        // Register the accept-waiter BEFORE publishing `start` so an early `accept`
        // (decoded on the event-loop task) is never lost.
        let await_accept = if needs_accept {
            let (accept_tx, accept_rx) = oneshot::channel();
            {
                let mut waiters = match self.accept_waiters.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                waiters.insert(token.to_string(), accept_tx);
            }
            Some(accept_rx)
        } else {
            None
        };

        // Per-frame publish: the start frame carries one-shot discovery tags; the
        // rest carry bare recipient tags. Mirrors the prepare+publish pair in `send`.
        let base = &self.base;
        let server_pubkey = self.server_pubkey;
        let mut start_tags = Some(start_tags);
        let publish = move |frame: JsonRpcNotification| {
            let tags = start_tags.take().unwrap_or_else(|| base_tags.clone());
            async move {
                let msg = JsonRpcMessage::Notification(frame);
                let (event_id, publishable) = base
                    .prepare_mcp_message(
                        &msg,
                        &server_pubkey,
                        CTXVM_MESSAGES_KIND,
                        tags,
                        None,
                        Some(gift_wrap_kind),
                    )
                    .await?;
                base.relay_pool.publish_event(&publishable).await?;
                Ok::<EventId, crate::core::error::Error>(event_id)
            }
        };

        let accept_timeout =
            Duration::from_millis(self.config.oversized_transfer.accept_timeout_ms);
        let result =
            send_oversized_transfer(frames, needs_accept, await_accept, accept_timeout, publish)
                .await;

        // Drop the accept-waiter entry regardless of outcome.
        if needs_accept {
            let mut waiters = match self.accept_waiters.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            waiters.remove(token);
        }

        let end_id = match result {
            Ok(id) => id,
            Err(error) => {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    server_pubkey = %self.server_pubkey.to_hex(),
                    method = ?message.method(),
                    "Failed to send oversized client request"
                );
                return Err(error);
            }
        };

        // Register the pending request against the END frame's event id.
        if let JsonRpcMessage::Request(ref req) = message {
            let is_initialize = req.method == INITIALIZE_METHOD;
            self.pending_requests
                .register(end_id.to_hex(), req.id.clone(), is_initialize)
                .await;
        }

        // Flip the one-shot discovery flag after a successful transfer.
        if discovery_sent {
            self.has_sent_discovery_tags.store(true, Ordering::Relaxed);
        }

        tracing::debug!(
            target: LOG_TARGET,
            end_event_id = %end_id.to_hex(),
            method = ?message.method(),
            "Sent oversized client request"
        );
        Ok(())
    }

    /// CEP-22: record the original `_meta.progressToken` value of an
    /// outbound request under its stringified form, replacing any stale entry
    /// for the same key. See [`Self::original_progress_tokens`].
    fn record_original_progress_token(&self, token: &str, original: &serde_json::Value) {
        let mut originals = match self.original_progress_tokens.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        originals.push(token.to_string(), original.clone());
    }

    /// CEP-22: drop — and return — the original `progressToken` value
    /// recorded for `token`, once its transfer concludes (delivered or failed).
    fn remove_original_progress_token(
        originals: &Mutex<LruCache<String, serde_json::Value>>,
        token: Option<&str>,
    ) -> Option<serde_json::Value> {
        let token = token?;
        let mut originals = match originals.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        originals.pop(token)
    }

    /// CEP-22: look up — without removing — the original `progressToken`
    /// value recorded for `token`, promoting its LRU recency so an in-flight
    /// transfer's record outlives idle ones.
    fn original_progress_token(
        originals: &Mutex<LruCache<String, serde_json::Value>>,
        token: &str,
    ) -> Option<serde_json::Value> {
        let mut originals = match originals.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        originals.get(token).cloned()
    }

    /// CEP-22: build the plain `notifications/progress` forwarded to the
    /// local consumer for an inbound oversized-transfer frame: `progress` is
    /// copied verbatim (plus `total`/`message` when present), the `cvm` frame
    /// payload is omitted, and `progressToken` is set to `original_token` —
    /// the value recorded at send time, NOT the frame's wire token. The
    /// wire stringifies every token, but rmcp's progress-watcher map is keyed
    /// by exact JSON type (`Number(5)` ≠ `String("5")`), so only the recorded
    /// original resets the requester's idle timer. Returns `None` when the
    /// frame has no `progress` (malformed; nothing worth forwarding).
    fn stripped_progress_notification(
        params: &serde_json::Value,
        original_token: &serde_json::Value,
    ) -> Option<JsonRpcMessage> {
        let mut stripped = serde_json::Map::new();
        stripped.insert("progressToken".to_string(), original_token.clone());
        stripped.insert("progress".to_string(), params.get("progress")?.clone());
        for key in ["total", "message"] {
            if let Some(value) = params.get(key) {
                stripped.insert(key.to_string(), value.clone());
            }
        }
        Some(JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: NOTIFICATIONS_PROGRESS_METHOD.to_string(),
            params: Some(serde_json::Value::Object(stripped)),
        }))
    }

    /// CEP-22: forward one stripped progress notification for
    /// the oversized frame `notif` onto the consumer channel, restoring the
    /// token recorded at send time. Falls back to the wire token for
    /// transfers with no record (e.g. a transfer addressed to a token this
    /// transport never sent); rmcp ignores tokens it never issued, so the
    /// fallback forward is harmless.
    fn forward_stripped_progress(
        notif: &JsonRpcNotification,
        token: &str,
        originals: &Mutex<LruCache<String, serde_json::Value>>,
        tx: &tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
    ) {
        let Some(params) = notif.params.as_ref() else {
            return;
        };
        let Some(original) = Self::original_progress_token(originals, token)
            .or_else(|| params.get("progressToken").cloned())
        else {
            return;
        };
        if let Some(stripped) = Self::stripped_progress_notification(params, &original) {
            let _ = tx.send(stripped);
        }
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
        oversized_receiver: Arc<Mutex<OversizedTransferReceiver>>,
        accept_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
        original_progress_tokens: Arc<Mutex<LruCache<String, serde_json::Value>>>,
        oversized_enabled: bool,
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                target: LOG_TARGET,
                                skipped = n,
                                "Relay broadcast lagged, skipping missed events"
                            );
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
                        &oversized_receiver,
                        &accept_waiters,
                        &original_progress_tokens,
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
                    // CEP-22: reap inbound transfers past their hard deadline.
                    // Local-only (no abort frame is emitted): the requester's
                    // own timeout fails the call, and late frames are
                    // orphan-ignored. `remove_expired` no-ops when
                    // `transfer_timeout_ms` is 0; the sync guard is dropped
                    // before anything awaits.
                    if oversized_enabled {
                        let reaped = {
                            let mut receiver = match oversized_receiver.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            receiver.remove_expired()
                        };
                        for token in reaped {
                            tracing::warn!(
                                target: LOG_TARGET,
                                token = %token,
                                "Oversized transfer reaped by watchdog"
                            );
                        }
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
        // CEP-22: advertise oversized-transfer support when enabled.
        if self.config.oversized_transfer.enabled {
            tags.push(Tag::custom(
                TagKind::Custom(tags::SUPPORT_OVERSIZED_TRANSFER.into()),
                Vec::<String>::new(),
            ));
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
        match stored.as_ref() {
            // First discovery-tag-carrying event becomes the session baseline.
            None => *stored = Some(event.clone()),
            // CEP-35 upgrade (mirrors TS `inbound-coordinator`): if the baseline was
            // captured from a non-initialize event (e.g. the first discovery tags
            // arrived on a notification) and this event carries a full
            // `InitializeResult` (has `protocolVersion`), upgrade the baseline to the
            // richer initialize response so `get_server_initialize_event` exposes the
            // full server identity/capabilities. Never downgrades.
            Some(existing) => {
                if !Self::event_has_initialize_result(existing)
                    && Self::event_has_initialize_result(event)
                {
                    *stored = Some(event.clone());
                }
            }
        }
    }

    /// Returns `true` when the event's `content` parses to a JSON-RPC response
    /// whose `result` is a full MCP `InitializeResult` (keyed on `protocolVersion`,
    /// matching the TS `InitializeResultSchema` marker).
    fn event_has_initialize_result(event: &Event) -> bool {
        serde_json::from_str::<serde_json::Value>(&event.content)
            .ok()
            .as_ref()
            .and_then(|content| content.get("result"))
            .and_then(|result| result.get("protocolVersion"))
            .is_some()
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
        oversized_receiver: &Arc<Mutex<OversizedTransferReceiver>>,
        accept_waiters: &Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
        original_progress_tokens: &Arc<Mutex<LruCache<String, serde_json::Value>>>,
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

        // CEP-22: intercept oversized-transfer frames ABOVE the correlation gate
        // below. This is mandatory: an `accept` is e-tagged to the start frame
        // (not in `pending`), and chunk/end response frames must be reassembled
        // rather than delivered raw. Plain `notifications/progress` (no `cvm`) and
        // ordinary responses fall through untouched.
        if let Ok(notif) = serde_json::from_str::<JsonRpcNotification>(&actual_event_content) {
            if notif.method == NOTIFICATIONS_PROGRESS_METHOD
                && OversizedTransferReceiver::is_oversized_frame(&notif)
            {
                // Token extraction accepts string or number — defensive only:
                // every known sender stringifies tokens into frames.
                let token = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("progressToken"))
                    .and_then(progress_token_string);

                // Route `accept` frames to the waiting sender by progressToken
                // (their e-tag is the start-frame id, which is not in `pending`).
                let is_accept = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("cvm"))
                    .and_then(OversizedFrame::from_cvm_value)
                    .is_some_and(|f| matches!(f, OversizedFrame::Accept));
                if is_accept {
                    if let Some(ref token) = token {
                        let waiter = {
                            let mut waiters = match accept_waiters.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            waiters.remove(token)
                        };
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(());
                            // The accept is the one inbound frame of a
                            // client→server upload — forward it (stripped) so
                            // the requester's idle timer re-arms for the
                            // response-wait phase. Only for a live waiter: a
                            // duplicate or stray accept must not poke the timer.
                            Self::forward_stripped_progress(
                                &notif,
                                token,
                                original_progress_tokens,
                                tx,
                            );
                        }
                    }
                    return;
                }

                // Touch the pending entry so the sweep does not evict the
                // request mid-transfer (chunks do not otherwise refresh it).
                if let Some(ref correlated_id) = e_tag {
                    pending.touch(correlated_id.as_str()).await;
                }

                // Feed the frame to the reassembler (process_frame is sync; the
                // guard is dropped before any await or channel send).
                let (outcome, tracked) = {
                    let mut receiver = match oversized_receiver.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    let outcome = receiver.process_frame(&notif);
                    // Zombie guard: forward progress only for transfers still
                    // tracked after this frame — a late/orphan frame must not
                    // keep a dead request's idle timer alive.
                    let tracked = token
                        .as_deref()
                        .is_some_and(|token| receiver.is_tracking(token));
                    (outcome, tracked)
                };
                match outcome {
                    // start/chunk consumed — forward a stripped (cvm-less)
                    // progress notification carrying the original token so the
                    // requester's progress-aware idle timeout resets.
                    Ok(None) => {
                        if tracked {
                            if let Some(ref token) = token {
                                Self::forward_stripped_progress(
                                    &notif,
                                    token,
                                    original_progress_tokens,
                                    tx,
                                );
                            }
                        }
                        return;
                    }
                    // end frame: deliver the reassembled (already-validated, may
                    // exceed 1 MB) message and clear the pending entry. No extra
                    // progress forward — the response itself resolves the request.
                    Ok(Some(message)) => {
                        if let Some(ref correlated_id) = e_tag {
                            pending.remove(correlated_id.as_str()).await;
                        } else {
                            // Matches the TS SDK: an oversized response that
                            // reassembles without a correlation `e` tag is still
                            // delivered (rmcp matches it by JSON-RPC id), but the
                            // missing transport-level correlation is worth a warn.
                            tracing::warn!(
                                target: LOG_TARGET,
                                "Oversized transfer completed without a correlation `e` tag; \
                                 delivering the reassembled response uncorrelated"
                            );
                        }
                        Self::remove_original_progress_token(
                            original_progress_tokens,
                            token.as_deref(),
                        );
                        let _ = tx.send(message);
                        return;
                    }
                    // Failure: clean up locally, let the request time out.
                    Err(error) => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            error = %error,
                            "Inbound oversized transfer failed"
                        );
                        Self::remove_original_progress_token(
                            original_progress_tokens,
                            token.as_deref(),
                        );
                        return;
                    }
                }
            }
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
        assert!(config.relay_urls.is_empty());
        assert!(config.server_pubkey.is_empty());
        assert_eq!(config.encryption_mode, EncryptionMode::Optional);
        assert_eq!(config.gift_wrap_mode, GiftWrapMode::Optional);
        assert!(!config.is_stateless);
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert!(config.discovery_relay_urls.is_none());
        assert!(config.fallback_operational_relay_urls.is_none());
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
            hinted_relay_urls: vec![],
            discovery_relay_urls: vec![],
            fallback_operational_relay_urls: vec![],
            pending_requests: ClientCorrelationStore::new(),
            has_sent_discovery_tags: AtomicBool::new(false),
            discovered_server_capabilities: Arc::new(Mutex::new(PeerCapabilities::default())),
            server_initialize_event: Arc::new(Mutex::new(None)),
            server_supports_ephemeral: Arc::new(AtomicBool::new(false)),
            seen_gift_wrap_ids: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(10).unwrap()))),
            oversized_receiver: Arc::new(Mutex::new(OversizedTransferReceiver::new())),
            accept_waiters: Arc::new(Mutex::new(HashMap::new())),
            original_progress_tokens: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(10).unwrap(),
            ))),
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
        // The oversized tag (default-on) is pushed last.
        assert_eq!(
            names,
            vec![
                "support_encryption",
                "support_encryption_ephemeral",
                "support_oversized_transfer"
            ]
        );
    }

    #[test]
    fn client_capability_tags_encryption_disabled() {
        let t = make_transport_for_tags(EncryptionMode::Disabled, GiftWrapMode::Optional);
        let tags = t.get_client_capability_tags();
        // No encryption tags; the default-on oversized tag remains.
        assert_eq!(tag_names(&tags), vec!["support_oversized_transfer"]);
    }

    #[test]
    fn client_capability_tags_persistent_gift_wrap() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Persistent);
        let tags = t.get_client_capability_tags();
        let names = tag_names(&tags);
        assert_eq!(
            names,
            vec!["support_encryption", "support_oversized_transfer"]
        );
    }

    #[test]
    fn client_capability_tags_oversized_enabled_by_default() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        assert!(t.config.oversized_transfer.enabled);
        let names = tag_names(&t.get_client_capability_tags());
        assert!(
            names.contains(&"support_oversized_transfer".to_string()),
            "oversized tag must be advertised by default"
        );
    }

    #[test]
    fn client_capability_tags_oversized_opt_out() {
        // The opt-out gate still works: disabling suppresses the tag.
        let mut t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        t.config.oversized_transfer = OversizedTransferConfig::default().with_enabled(false);
        let names = tag_names(&t.get_client_capability_tags());
        assert!(
            !names.contains(&"support_oversized_transfer".to_string()),
            "oversized tag must not be advertised when disabled"
        );
    }

    #[test]
    fn client_capability_tags_oversized_enabled() {
        let mut t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        t.config.oversized_transfer.enabled = true;
        let names = tag_names(&t.get_client_capability_tags());
        assert!(
            names.contains(&"support_oversized_transfer".to_string()),
            "oversized tag must be advertised when enabled"
        );
    }

    #[test]
    fn client_capability_tags_oversized_enabled_without_encryption() {
        // Tag is emitted independently of the encryption capability tags.
        let mut t = make_transport_for_tags(EncryptionMode::Disabled, GiftWrapMode::Optional);
        t.config.oversized_transfer.enabled = true;
        let names = tag_names(&t.get_client_capability_tags());
        assert_eq!(names, vec!["support_oversized_transfer"]);
    }

    #[test]
    fn client_config_oversized_builders() {
        let cfg = NostrClientTransportConfig::default().with_oversized_enabled(true);
        assert!(cfg.oversized_transfer.enabled);
        let cfg = NostrClientTransportConfig::default()
            .with_oversized_transfer(OversizedTransferConfig::enabled().with_chunk_size(1024));
        assert!(cfg.oversized_transfer.enabled);
        assert_eq!(cfg.oversized_transfer.chunk_size, 1024);
    }

    // ── CEP-22 original progressToken record/restore ─────────────

    #[test]
    fn original_progress_token_roundtrip_preserves_numeric_type() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        // rmcp stamps numeric tokens; the record keys them by stringified form.
        t.record_original_progress_token("7", &serde_json::json!(7));
        let restored = NostrClientTransport::remove_original_progress_token(
            &t.original_progress_tokens,
            Some("7"),
        );
        assert_eq!(restored, Some(serde_json::json!(7)));
        // Dropped on first take — the transfer concluded.
        assert_eq!(
            NostrClientTransport::remove_original_progress_token(
                &t.original_progress_tokens,
                Some("7"),
            ),
            None
        );
    }

    #[test]
    fn original_progress_token_string_never_parsed_to_number() {
        // A legitimate String("5") token must restore as a string — restoring
        // by parsing numeric-looking wire strings would corrupt it.
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        t.record_original_progress_token("5", &serde_json::json!("5"));
        assert_eq!(
            NostrClientTransport::remove_original_progress_token(
                &t.original_progress_tokens,
                Some("5"),
            ),
            Some(serde_json::json!("5"))
        );
    }

    #[test]
    fn remove_original_progress_token_handles_missing() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        assert_eq!(
            NostrClientTransport::remove_original_progress_token(&t.original_progress_tokens, None,),
            None
        );
        assert_eq!(
            NostrClientTransport::remove_original_progress_token(
                &t.original_progress_tokens,
                Some("unknown"),
            ),
            None
        );
    }

    /// `send()` must record the original token value for every
    /// oversized-eligible request — including sub-threshold ones, whose
    /// *responses* may still come back fragmented.
    #[tokio::test]
    async fn send_records_numeric_progress_token_original() {
        let mut t = make_transport_for_tags(EncryptionMode::Disabled, GiftWrapMode::Optional);
        t.config.oversized_transfer.enabled = true;
        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({ "_meta": { "progressToken": 7 } })),
        });
        t.send(&request).await.expect("send small request");

        let recorded = NostrClientTransport::remove_original_progress_token(
            &t.original_progress_tokens,
            Some("7"),
        );
        assert_eq!(
            recorded,
            Some(serde_json::json!(7)),
            "numeric token must be recorded under its stringified form"
        );
    }

    /// With oversized transfer disabled (explicit opt-out) nothing is recorded.
    #[tokio::test]
    async fn send_records_nothing_when_oversized_disabled() {
        let mut t = make_transport_for_tags(EncryptionMode::Disabled, GiftWrapMode::Optional);
        t.config.oversized_transfer = OversizedTransferConfig::default().with_enabled(false);
        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({ "_meta": { "progressToken": 7 } })),
        });
        t.send(&request).await.expect("send small request");

        assert_eq!(
            NostrClientTransport::remove_original_progress_token(
                &t.original_progress_tokens,
                Some("7"),
            ),
            None
        );
    }

    // ── CEP-22 stripped progress construction ────────────────────

    #[test]
    fn stripped_progress_notification_strips_cvm_and_restores_token() {
        let params = serde_json::json!({
            "progressToken": "7",
            "progress": 3,
            "total": 5,
            "message": "transferring",
            "cvm": { "type": "oversized-transfer", "frameType": "chunk", "data": "x" },
        });
        let stripped =
            NostrClientTransport::stripped_progress_notification(&params, &serde_json::json!(7))
                .expect("frame carries progress");
        let JsonRpcMessage::Notification(n) = stripped else {
            panic!("expected a notification");
        };
        assert_eq!(n.method, NOTIFICATIONS_PROGRESS_METHOD);
        let p = n.params.expect("params");
        assert_eq!(
            p["progressToken"],
            serde_json::json!(7),
            "token must be the restored original, not the wire string"
        );
        assert_eq!(p["progress"], serde_json::json!(3));
        assert_eq!(p["total"], serde_json::json!(5));
        assert_eq!(p["message"], serde_json::json!("transferring"));
        assert!(p.get("cvm").is_none(), "cvm payload must be stripped");
    }

    #[test]
    fn stripped_progress_notification_requires_progress_and_omits_absent_fields() {
        // No `progress` → nothing worth forwarding.
        let malformed = serde_json::json!({ "progressToken": "7", "cvm": {} });
        assert!(NostrClientTransport::stripped_progress_notification(
            &malformed,
            &serde_json::json!(7)
        )
        .is_none());

        // Absent total/message are omitted, not nulled.
        let minimal = serde_json::json!({ "progressToken": "7", "progress": 1 });
        let stripped =
            NostrClientTransport::stripped_progress_notification(&minimal, &serde_json::json!("7"))
                .expect("progress present");
        let JsonRpcMessage::Notification(n) = stripped else {
            panic!("expected a notification");
        };
        let p = n.params.expect("params");
        let keys = p.as_object().expect("object params");
        assert_eq!(keys.len(), 2, "only progressToken + progress: {p}");
        assert_eq!(p["progressToken"], serde_json::json!("7"));
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
        make_event_with_content_and_tags("{}", tag_parts)
    }

    fn make_event_with_content_and_tags(content: &str, tag_parts: &[&[&str]]) -> Event {
        let keys = Keys::generate();
        let tags: Vec<Tag> = tag_parts.iter().map(|p| make_tag(p)).collect();
        let builder = EventBuilder::new(Kind::Custom(CTXVM_MESSAGES_KIND), content).tags(tags);
        let unsigned = builder.build(keys.public_key());
        unsigned.sign_with_keys(&keys).unwrap()
    }

    /// A JSON-RPC response carrying a full `InitializeResult` (has `protocolVersion`).
    fn initialize_result_content() -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": { "name": "UpgradedServer", "version": "1.0.0" }
            }
        })
        .to_string()
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

    #[test]
    fn client_baseline_upgraded_to_initialize_result() {
        let caps = Mutex::new(PeerCapabilities::default());
        let init = Mutex::new(None);

        // First discovery tags arrive on a non-initialize event (e.g. a notification).
        let baseline = make_event_with_tags(&[&["support_encryption"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &baseline);
        assert_eq!(init.lock().unwrap().as_ref().unwrap().id, baseline.id);

        // A later event carries a full InitializeResult → baseline is upgraded.
        let init_event = make_event_with_content_and_tags(
            &initialize_result_content(),
            &[&["support_encryption"]],
        );
        NostrClientTransport::learn_server_discovery(&caps, &init, &init_event);
        assert_eq!(
            init.lock().unwrap().as_ref().unwrap().id,
            init_event.id,
            "baseline must upgrade to the initialize-result event"
        );

        // A still-later non-initialize event must NOT downgrade the baseline.
        let later = make_event_with_tags(&[&["support_encryption_ephemeral"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &later);
        assert_eq!(
            init.lock().unwrap().as_ref().unwrap().id,
            init_event.id,
            "baseline must not downgrade away from the initialize result"
        );
    }
}
