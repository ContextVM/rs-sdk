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
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
use crate::transport::open_stream::{
    open_stream_frame_from_notification, FrameOutcome, KeepaliveAction, OnAbortHook, OnCloseHook,
    OpenStreamConfig, OpenStreamFrame, OpenStreamReceiver, OpenStreamRegistryPolicy,
    OpenStreamWriter, OpenStreamWriterOptions, PublishFrame,
};
use crate::transport::oversized_transfer::{
    build_oversized_frames, progress_token_string, resolve_safe_chunk_size, OversizedFrame,
    OversizedSenderOptions, OversizedTransferConfig, OversizedTransferReceiver, TransferPolicy,
    ACCEPT_PROGRESS,
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

/// CEP-41: the `support_open_stream` capability tag to advertise, or empty when
/// open-stream is disabled. Mirrors [`oversized_support_tags`].
fn open_stream_support_tags(config: &OpenStreamConfig) -> Vec<Tag> {
    if config.enabled {
        vec![Tag::custom(
            TagKind::Custom(tags::SUPPORT_OPEN_STREAM.into()),
            Vec::<String>::new(),
        )]
    } else {
        Vec::new()
    }
}

/// CEP-22 + CEP-41: the internal capability tags advertised on announcements and
/// replayed on the first response to each client.
fn internal_common_capability_tags(config: &NostrServerTransportConfig) -> Vec<Tag> {
    let mut tags = oversized_support_tags(config);
    tags.extend(open_stream_support_tags(&config.open_stream));
    tags
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

/// CEP-41: build the empty per-peer open-stream reader store, bounded to
/// `max_sessions` peers. Mirrors [`new_oversized_receiver_store`]; one
/// [`OpenStreamReceiver`] per client pubkey is inserted lazily for inbound
/// (client→server) streams.
///
/// Uses a [`tokio::sync::Mutex`] rather than an `RwLock`: the registry's
/// `FnOnce` lifecycle hooks are `Send` but not `Sync`, so a shared-read lock
/// could not be made `Sync`; the store is write-only anyway (`process_frame`
/// needs `&mut`), so exclusive access loses nothing.
fn new_open_stream_receiver_store(
    max_sessions: usize,
) -> Arc<AsyncMutex<LruCache<String, OpenStreamReceiver>>> {
    Arc::new(AsyncMutex::new(LruCache::new(
        NonZeroUsize::new(max_sessions).unwrap_or(NonZeroUsize::new(1).unwrap()),
    )))
}

/// CEP-41: response-routing fields captured at writer creation, while the
/// request's event route is fresh.
///
/// The deferred final response is delivered from this snapshot (via
/// [`NostrServerTransport::send_open_stream_deferred_response`]) rather than from
/// `event_routes`, so a stream that outlives `request_timeout` — after which the
/// route is swept — still delivers its response. `mirrored_wrap_kind` mirrors
/// the inbound gift-wrap kind for CEP-19, exactly as `send_response` does.
#[derive(Clone)]
struct RouteSnapshot {
    client_pubkey: PublicKey,
    original_request_id: serde_json::Value,
    is_encrypted: bool,
    mirrored_wrap_kind: Option<u16>,
}

/// CEP-41: the per-stream coordination slot for a server→client writer, keyed by
/// request `event_id` in [`ServerOpenStreamState::slots`].
///
/// A single mutex over the whole map serializes the two writers of the deferred
/// final response — `send_response` (the worker task) and the writer's
/// close/abort hook (the tool task) — against the [`terminated`](Self::terminated)
/// flag, so the response is never both stashed *and* dropped under a race.
struct OpenStreamSlot {
    writer: OpenStreamWriter,
    snapshot: RouteSnapshot,
    /// The final response, stashed by `send_response` when it arrives before the
    /// stream closes (ordering A).
    pending_response: Option<JsonRpcMessage>,
    /// Set by the writer's close/abort hook once the stream is terminal. When
    /// `send_response` arrives after this (ordering B), it delivers immediately.
    terminated: bool,
}

/// CEP-41: the open-stream runtime state shared between the server transport and
/// its spawned event loop. Bundled so the event-loop signature stays manageable.
#[derive(Clone)]
struct ServerOpenStreamState {
    /// Master gate (`config.open_stream.enabled`).
    enabled: bool,
    /// Reader admission/buffering/keepalive policy projected from config.
    policy: OpenStreamRegistryPolicy,
    /// Per-peer reader engines for inbound (client→server) streams.
    receiver: Arc<AsyncMutex<LruCache<String, OpenStreamReceiver>>>,
    /// Per-stream writer + deferred-response slots, keyed by `event_id`.
    slots: Arc<Mutex<HashMap<String, OpenStreamSlot>>>,
    /// `progress_token → event_id`, so inbound control frames and the keepalive
    /// sweep resolve the writer/route without consulting the route store.
    token_to_event: Arc<Mutex<HashMap<String, String>>>,
    /// Monotonic `progress` source for server-*as-reader* control frames
    /// (`accept`/`pong`/`ping` on inbound client→server streams, where no writer
    /// owns the counter). Per-token monotonicity holds even though it is shared.
    control_progress: Arc<AtomicU64>,
}

impl ServerOpenStreamState {
    fn new(config: &OpenStreamConfig, max_sessions: usize) -> Self {
        Self {
            enabled: config.enabled,
            policy: config.into(),
            receiver: new_open_stream_receiver_store(max_sessions),
            slots: Arc::new(Mutex::new(HashMap::new())),
            token_to_event: Arc::new(Mutex::new(HashMap::new())),
            control_progress: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Next monotonic control-frame `progress` (1, 2, 3, …) for the reader path.
    fn next_control_progress(&self) -> u64 {
        self.control_progress.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Lock-poison-tolerant access to the slots map.
    fn lock_slots(&self) -> std::sync::MutexGuard<'_, HashMap<String, OpenStreamSlot>> {
        match self.slots.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn lock_token_index(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        match self.token_to_event.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Clone the active writer for `event_id`, if any (for inbound ping/abort
    /// routing and the worker's extensions injection).
    fn writer_for(&self, event_id: &str) -> Option<OpenStreamWriter> {
        self.lock_slots().get(event_id).map(|s| s.writer.clone())
    }

    /// Resolve `progress_token → event_id`.
    fn event_id_for_token(&self, token: &str) -> Option<String> {
        self.lock_token_index().get(token).cloned()
    }
}

/// CEP-41: the outcome of the response-deferral decision in `send_response`.
enum OpenStreamDeferral {
    /// The response was stashed; the writer's close/abort hook will flush it.
    Deferred,
    /// The stream is already terminal — deliver this response now from the snapshot.
    SendNow {
        snapshot: RouteSnapshot,
        response: JsonRpcMessage,
    },
    /// No active stream for this event — send the response through the normal path.
    Passthrough(JsonRpcMessage),
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
    /// CEP-22 oversized payload transfer configuration. Enabled by default.
    pub oversized_transfer: OversizedTransferConfig,
    /// CEP-41 open-stream configuration. Disabled by default (opt-in).
    ///
    /// When enabled, drives capability advertisement/learning, server→client
    /// writers, response deferral, and the keepalive sweep. Opt in with
    /// `OpenStreamConfig::enabled()` / `with_enabled(true)`.
    pub open_stream: OpenStreamConfig,
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
            open_stream: OpenStreamConfig::default(),
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
    /// CEP-41: open-stream runtime state (writers, deferred responses, per-peer
    /// reader engines, `progress_token → event_id` index). Inert when
    /// `open_stream.enabled` is `false`.
    open_stream: ServerOpenStreamState,
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
    /// Set the full CEP-41 open-stream configuration (disabled by default; opt in
    /// with `OpenStreamConfig::enabled()`).
    pub fn with_open_stream(mut self, config: OpenStreamConfig) -> Self {
        self.open_stream = config;
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
        // CEP-22 + CEP-41: advertise oversized-transfer and open-stream support in
        // announcements + first responses (each gated by its own config flag).
        announcement_manager.set_internal_common_tags(internal_common_capability_tags(&config));
        Ok(Self {
            announcement_manager,
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            sessions: SessionStore::with_capacity(config.max_sessions),
            oversized_receiver: new_oversized_receiver_store(config.max_sessions),
            open_stream: ServerOpenStreamState::new(&config.open_stream, config.max_sessions),
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
        // CEP-22 + CEP-41: advertise oversized-transfer and open-stream support in
        // announcements + first responses (each gated by its own config flag).
        announcement_manager.set_internal_common_tags(internal_common_capability_tags(&config));
        Ok(Self {
            announcement_manager,
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            sessions: SessionStore::with_capacity(config.max_sessions),
            oversized_receiver: new_oversized_receiver_store(config.max_sessions),
            open_stream: ServerOpenStreamState::new(&config.open_stream, config.max_sessions),
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
        let open_stream = self.open_stream.clone();
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
                open_stream,
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
        // CEP-41: dispose every inbound reader session and drop all writer /
        // deferred-response state.
        {
            let mut receivers = self.open_stream.receiver.lock().await;
            for (_, receiver) in receivers.iter_mut() {
                receiver.clear();
            }
            receivers.clear();
        }
        self.open_stream.lock_slots().clear();
        self.open_stream.lock_token_index().clear();
        Ok(())
    }

    /// Send a response back to the client that sent the original request.
    pub async fn send_response(&self, event_id: &str, mut response: JsonRpcMessage) -> Result<()> {
        // CEP-41: response deferral. Decide BEFORE consuming the route — for a
        // started stream the final response rides the captured snapshot, not the
        // (possibly-swept) event route.
        if self.open_stream.enabled {
            match self.try_defer_open_stream_response(event_id, response) {
                // Stashed (ordering A) — the close/abort hook flushes it later.
                OpenStreamDeferral::Deferred => return Ok(()),
                // Stream already closed (ordering B) — deliver from the snapshot now.
                OpenStreamDeferral::SendNow { snapshot, response } => {
                    return self
                        .send_open_stream_deferred_response(event_id, &snapshot, response)
                        .await;
                }
                // No active stream for this event — fall through to the normal path.
                OpenStreamDeferral::Passthrough(returned) => response = returned,
            }
        }

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

    /// CEP-41: clone the active writer for `event_id` so the rmcp worker can inject
    /// it into the request's `extensions` typemap before dispatch. Returns
    /// `None` when open-stream is disabled or no writer exists for this request.
    #[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
    pub(crate) fn get_open_stream_writer(&self, event_id: &str) -> Option<OpenStreamWriter> {
        if !self.open_stream.enabled {
            return None;
        }
        self.open_stream.writer_for(event_id)
    }

    /// CEP-41: decide how `send_response` should handle the final response for a
    /// (possibly) streaming request. Run under the slots lock so the stash/flush
    /// decision is consistent against the close/abort hook's `terminated` flag.
    fn try_defer_open_stream_response(
        &self,
        event_id: &str,
        response: JsonRpcMessage,
    ) -> OpenStreamDeferral {
        let mut slots = self.open_stream.lock_slots();
        let Some(slot) = slots.get_mut(event_id) else {
            return OpenStreamDeferral::Passthrough(response);
        };

        if !slot.writer.has_started() {
            // The request carried a progressToken but the tool never streamed.
            // Drop the writer and send normally (progress-token-conflict guard —
            // a deferred-but-never-closed stream would otherwise hang the response).
            let token = slot.writer.progress_token().to_string();
            slots.remove(event_id);
            drop(slots);
            self.open_stream.lock_token_index().remove(&token);
            return OpenStreamDeferral::Passthrough(response);
        }

        if slot.terminated {
            // Ordering B (the common case): the stream already closed/aborted —
            // deliver now from the captured snapshot (the route may be swept).
            let snapshot = slot.snapshot.clone();
            let token = slot.writer.progress_token().to_string();
            slots.remove(event_id);
            drop(slots);
            self.open_stream.lock_token_index().remove(&token);
            OpenStreamDeferral::SendNow { snapshot, response }
        } else {
            // Ordering A: the stream is still open — hold the response; the
            // close/abort hook flushes it from the snapshot when the stream ends.
            slot.pending_response = Some(response);
            OpenStreamDeferral::Deferred
        }
    }

    /// CEP-41: deliver a deferred final response from a captured [`RouteSnapshot`],
    /// never consulting `event_routes` (route-lifetime-independent; the route may already be gone).
    async fn send_open_stream_deferred_response(
        &self,
        event_id: &str,
        snapshot: &RouteSnapshot,
        response: JsonRpcMessage,
    ) -> Result<()> {
        Self::publish_open_stream_deferred_response(
            &self.base,
            self.config.gift_wrap_mode,
            event_id,
            snapshot,
            response,
        )
        .await
    }

    /// CEP-41 (static): the actual deferred-response publish, callable from both the
    /// `&self` path and the writer's close/abort hook (which has no `self`).
    async fn publish_open_stream_deferred_response(
        base: &BaseTransport,
        gift_wrap_mode: GiftWrapMode,
        event_id: &str,
        snapshot: &RouteSnapshot,
        mut response: JsonRpcMessage,
    ) -> Result<()> {
        // Restore the original request id (the normal path restores it from the
        // popped route; here it comes from the snapshot).
        match &mut response {
            JsonRpcMessage::Response(r) => r.id = snapshot.original_request_id.clone(),
            JsonRpcMessage::ErrorResponse(r) => r.id = snapshot.original_request_id.clone(),
            _ => {}
        }
        let event_id_parsed = EventId::from_hex(event_id).map_err(|error| {
            Error::Other(format!("Invalid event id for deferred response: {error}"))
        })?;
        // Correlate via the `e` tag exactly like a normal response so the client's
        // correlation gate accepts it.
        let tags = BaseTransport::create_response_tags(&snapshot.client_pubkey, &event_id_parsed);
        let gift_wrap_kind = Self::select_outbound_gift_wrap_kind(
            gift_wrap_mode,
            snapshot.is_encrypted,
            snapshot.mirrored_wrap_kind,
        );
        base.send_mcp_message(
            &response,
            &snapshot.client_pubkey,
            CTXVM_MESSAGES_KIND,
            tags,
            Some(snapshot.is_encrypted),
            gift_wrap_kind,
        )
        .await
        .map(|_| ())
    }

    /// CEP-41 (static): the writer's close/abort hook. Marks the stream terminal
    /// and, when the response already arrived (ordering A), flushes it from the
    /// snapshot. Ordering B leaves the terminal slot for `send_response`.
    async fn flush_open_stream_response(
        state: &ServerOpenStreamState,
        base: &BaseTransport,
        gift_wrap_mode: GiftWrapMode,
        event_id: &str,
    ) {
        let ready = {
            let mut slots = state.lock_slots();
            match slots.get_mut(event_id) {
                Some(slot) => {
                    slot.terminated = true;
                    slot.pending_response.take().map(|response| {
                        (
                            slot.snapshot.clone(),
                            slot.writer.progress_token().to_string(),
                            response,
                        )
                    })
                }
                None => None,
            }
        };

        let Some((snapshot, token, response)) = ready else {
            // Ordering B: the response has not arrived yet. Leave the terminal slot
            // in place; `send_response` will deliver it from the snapshot.
            return;
        };

        // Ordering A: the response was stashed before the stream closed — remove
        // the slot and deliver it now.
        state.lock_slots().remove(event_id);
        state.lock_token_index().remove(&token);
        if let Err(error) = Self::publish_open_stream_deferred_response(
            base,
            gift_wrap_mode,
            event_id,
            &snapshot,
            response,
        )
        .await
        {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                event_id = %event_id,
                "Failed to flush deferred open-stream response"
            );
        }
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
        self.spawn_discoverability_publication();
    }

    /// Spawn profile metadata and relay-list publication for direct transport users.
    ///
    /// This publishes kind 0 and kind 10002 discoverability events when configured.
    /// It intentionally does not spawn CEP-6 capability announcement tasks because
    /// those inject synthetic MCP requests that require an rmcp worker.
    pub fn spawn_discoverability_publication(&mut self) {
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

    /// CEP-22: one watchdog sweep over the per-peer reassembly engines. Reaps
    /// transfers past their hard deadline — local-only (no abort frame is
    /// emitted): the requester's own timeout fails the call, and late frames
    /// are orphan-ignored — then drops now-empty receivers so long-gone peers
    /// stop pinning LRU slots (admission recreates them on demand).
    async fn sweep_oversized_receivers(
        oversized_receiver: &Arc<RwLock<LruCache<String, OversizedTransferReceiver>>>,
    ) {
        let mut receivers = oversized_receiver.write().await;
        let mut empty_peers: Vec<String> = Vec::new();
        for (peer, receiver) in receivers.iter_mut() {
            for token in receiver.remove_expired() {
                tracing::warn!(
                    target: LOG_TARGET,
                    client_pubkey = %peer,
                    token = %token,
                    "Oversized transfer reaped by watchdog"
                );
            }
            if receiver.active_transfer_count() == 0 {
                empty_peers.push(peer.clone());
            }
        }
        // Keys collected first, popped after: never mutate mid-iteration.
        for peer in empty_peers {
            receivers.pop(&peer);
        }
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
        open_stream: ServerOpenStreamState,
        cancel: CancellationToken,
    ) {
        let mut notifications = relay_pool.notifications();

        // CEP-22: receiver-side watchdog sweep. Same clamp formula as the
        // client's correlation sweep; the arm is disabled entirely when the
        // feature is off or the deadline is 0 (no watchdog).
        let watchdog_enabled = oversized_enabled && transfer_policy.transfer_timeout_ms != 0;
        let sweep_interval = (Duration::from_millis(transfer_policy.transfer_timeout_ms) / 2)
            .clamp(Duration::from_secs(1), Duration::from_secs(30));
        let mut sweep_timer =
            tokio::time::interval_at(tokio::time::Instant::now() + sweep_interval, sweep_interval);

        // CEP-41: keepalive sweep for server-as-reader sessions. Cadence = half the
        // idle timeout, clamped to [1s, 30s] (the idle→probe→abort machine only
        // needs sub-idle granularity). Armed only when open-stream is enabled.
        let open_stream_sweep_enabled =
            open_stream.enabled && open_stream.policy.idle_timeout_ms != 0;
        let open_stream_sweep_interval =
            (Duration::from_millis(open_stream.policy.idle_timeout_ms) / 2)
                .clamp(Duration::from_secs(1), Duration::from_secs(30));
        let mut open_stream_sweep_timer = tokio::time::interval_at(
            tokio::time::Instant::now() + open_stream_sweep_interval,
            open_stream_sweep_interval,
        );

        loop {
            let notification = tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(
                        target: LOG_TARGET,
                        "Server event loop cancelled"
                    );
                    break;
                }
                _ = sweep_timer.tick(), if watchdog_enabled => {
                    Self::sweep_oversized_receivers(&oversized_receiver).await;
                    continue;
                }
                _ = open_stream_sweep_timer.tick(), if open_stream_sweep_enabled => {
                    Self::sweep_open_stream_sessions(
                        &open_stream,
                        &relay_pool,
                        encryption_mode,
                        gift_wrap_mode,
                    )
                    .await;
                    continue;
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
                // CEP-22: snapshot the flag BEFORE the learning gate mutates
                // it — the very `start` frame carries the client's support tag, so
                // without this snapshot the first transfer would never get an `accept`.
                let client_already_supported = session.supports_oversized_transfer;
                // CEP-22: only learn oversized support if it is enabled on this server.
                session.supports_oversized_transfer |=
                    oversized_enabled && discovered.supports_oversized_transfer;
                // CEP-41: learn the client's open-stream support (gated on enabled).
                // Captured AFTER the OR-learn so the very `start` frame that carries
                // the support tag still elicits an `accept`.
                session.supports_open_stream |=
                    open_stream.enabled && discovered.supports_open_stream;
                let client_supports_open_stream = session.supports_open_stream;

                // CEP-22: intercept oversized-transfer frames before request
                // correlation/dispatch. A disabled server forwards raw progress
                // notifications as before.
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
                                &open_stream,
                            )
                            .await;
                            continue;
                        }
                    }
                }

                // CEP-41: intercept open-stream frames beside the oversized branch.
                // Type-disjoint from oversized (`is_open_stream_frame` vs
                // `is_oversized_frame` claim distinct `cvm.type`s), so order is
                // irrelevant. A disabled server forwards the raw notification.
                if open_stream.enabled {
                    if let JsonRpcMessage::Notification(ref n) = mcp_msg {
                        if OpenStreamReceiver::is_open_stream_frame(n) {
                            drop(sessions_w);
                            Self::handle_open_stream_frame(
                                &open_stream,
                                &relay_pool,
                                encryption_mode,
                                gift_wrap_mode,
                                n,
                                &sender_pubkey,
                                &event_id,
                                is_encrypted,
                                is_gift_wrap,
                                outer_kind,
                                client_supports_open_stream,
                            )
                            .await;
                            continue;
                        }
                    }
                }

                // Track request for correlation
                if let JsonRpcMessage::Request(ref req) = mcp_msg {
                    let original_id = req.id.clone();

                    // Extract progress token from _meta if present. String or
                    // number (rmcp issues numbers): without numeric acceptance
                    // the response eligibility gate in `send_response` never
                    // opens for rmcp clients. Normalized to its stringified form
                    // for routing and frame addressing (the wire keeps emitting
                    // string tokens).
                    let progress_token = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                        .and_then(progress_token_string);

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

                    // CEP-41: capture the route fields for the writer's snapshot
                    // BEFORE they are moved into the route store.
                    let writer_request_id = original_id.clone();
                    let writer_token = progress_token.clone();

                    event_routes
                        .register(
                            event_id.clone(),
                            sender_pubkey.clone(),
                            original_id,
                            progress_token,
                        )
                        .await;

                    // CEP-41: a `tools/call` carrying a progressToken gets a
                    // server→client writer, captured with a route snapshot (so the
                    // deferred response survives a route sweep) and injected into
                    // the tool via the rmcp request extensions.
                    if open_stream.enabled && req.method == "tools/call" {
                        if let Some(token) = writer_token {
                            Self::create_open_stream_writer(
                                &open_stream,
                                &relay_pool,
                                encryption_mode,
                                gift_wrap_mode,
                                &event_id,
                                &sender_pubkey,
                                &token,
                                writer_request_id,
                                is_encrypted,
                                if is_gift_wrap { Some(outer_kind) } else { None },
                            );
                        }
                    }
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
    /// known, feeds the frame to this peer's reassembler, and — on the
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
        open_stream: &ServerOpenStreamState,
    ) {
        // The outer progressToken keys the transfer (needed for accept + route).
        // String or number — defensive only: every known sender stringifies
        // tokens into frames.
        let token = frame
            .params
            .as_ref()
            .and_then(|p| p.get("progressToken"))
            .and_then(progress_token_string);

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
                // CEP-41: extract the writer info from the reassembled `tools/call`
                // before `message` is moved. The oversized reassembly path bypasses
                // the regular request path, so the writer must be created HERE too
                // (mirrors TS `handleIncomingRequest`, which oversized re-enters).
                let writer_token = match &message {
                    JsonRpcMessage::Request(req) if req.method == "tools/call" => req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                        .and_then(progress_token_string),
                    _ => None,
                };
                let writer_request_id = original_id.clone();
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
                if open_stream.enabled {
                    if let Some(progress_token) = writer_token {
                        Self::create_open_stream_writer(
                            open_stream,
                            relay_pool,
                            encryption_mode,
                            gift_wrap_mode,
                            event_id,
                            sender_pubkey,
                            &progress_token,
                            writer_request_id,
                            is_encrypted,
                            if is_gift_wrap { Some(outer_kind) } else { None },
                        );
                    }
                }
                let _ = tx.send(IncomingRequest {
                    message,
                    client_pubkey: sender_pubkey.to_string(),
                    event_id: event_id.to_string(),
                    is_encrypted,
                });
            }
            // Clean up locally, let the peer's own timeout fire.
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
        let accept = match OversizedFrame::Accept.into_progress_notification(
            token,
            ACCEPT_PROGRESS,
            Some("oversized request accepted"),
        ) {
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

    /// CEP-41: create a server→client [`OpenStreamWriter`] for a `tools/call`
    /// carrying a `progressToken`, capture its [`RouteSnapshot`], register the
    /// `progress_token → event_id` index, and store the slot. The writer is later
    /// injected into the tool via the rmcp request `extensions`; its
    /// close/abort hooks flush the deferred final response from the snapshot.
    #[allow(clippy::too_many_arguments)]
    fn create_open_stream_writer(
        state: &ServerOpenStreamState,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        event_id: &str,
        client_pubkey_hex: &str,
        progress_token: &str,
        original_request_id: serde_json::Value,
        is_encrypted: bool,
        mirrored_wrap_kind: Option<u16>,
    ) {
        let client_pubkey = match PublicKey::from_hex(client_pubkey_hex) {
            Ok(pk) => pk,
            Err(_) => return,
        };
        let event_id_parsed = match EventId::from_hex(event_id) {
            Ok(id) => id,
            Err(_) => return,
        };
        let gift_wrap_kind =
            Self::select_outbound_gift_wrap_kind(gift_wrap_mode, is_encrypted, mirrored_wrap_kind);

        // Publish closure: every frame is e-tagged to the request event (so the
        // client can keep its pending correlation alive) and mirrors the inbound
        // gift-wrap kind (CEP-19). `send_notification`'s one-shot discovery tags
        // are not replayed — they already rode the initialize response / stream
        // start by the time a tool streams.
        let publish_relay_pool = Arc::clone(relay_pool);
        let publish_frame: PublishFrame = Arc::new(move |notification: JsonRpcNotification| {
            let relay_pool = Arc::clone(&publish_relay_pool);
            Box::pin(async move {
                let base = BaseTransport {
                    relay_pool,
                    encryption_mode,
                    is_connected: true,
                };
                let tags = BaseTransport::create_response_tags(&client_pubkey, &event_id_parsed);
                let message = JsonRpcMessage::Notification(notification);
                base.send_mcp_message(
                    &message,
                    &client_pubkey,
                    CTXVM_MESSAGES_KIND,
                    tags,
                    Some(is_encrypted),
                    gift_wrap_kind,
                )
                .await
            })
        });

        // Terminal hooks flush any deferred final response from the snapshot.
        let on_close: OnCloseHook = {
            let state = state.clone();
            let relay_pool = Arc::clone(relay_pool);
            let event_id = event_id.to_string();
            Arc::new(move || {
                let state = state.clone();
                let relay_pool = Arc::clone(&relay_pool);
                let event_id = event_id.clone();
                Box::pin(async move {
                    let base = BaseTransport {
                        relay_pool,
                        encryption_mode,
                        is_connected: true,
                    };
                    Self::flush_open_stream_response(&state, &base, gift_wrap_mode, &event_id)
                        .await;
                })
            })
        };
        let on_abort: OnAbortHook = {
            let state = state.clone();
            let relay_pool = Arc::clone(relay_pool);
            let event_id = event_id.to_string();
            Arc::new(move |_reason| {
                let state = state.clone();
                let relay_pool = Arc::clone(&relay_pool);
                let event_id = event_id.clone();
                Box::pin(async move {
                    let base = BaseTransport {
                        relay_pool,
                        encryption_mode,
                        is_connected: true,
                    };
                    Self::flush_open_stream_response(&state, &base, gift_wrap_mode, &event_id)
                        .await;
                })
            })
        };

        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: progress_token.to_string(),
            publish_frame,
            content_type: None,
            on_close: Some(on_close),
            on_abort: Some(on_abort),
        });
        let snapshot = RouteSnapshot {
            client_pubkey,
            original_request_id,
            is_encrypted,
            mirrored_wrap_kind,
        };
        state.lock_slots().insert(
            event_id.to_string(),
            OpenStreamSlot {
                writer,
                snapshot,
                pending_response: None,
                terminated: false,
            },
        );
        state
            .lock_token_index()
            .insert(progress_token.to_string(), event_id.to_string());
    }

    /// CEP-41 inbound interception (beside the oversized branch). Routes control
    /// frames to the active writer (`ping → pong`, `abort → abort`) and otherwise
    /// drives the server-as-reader engine (`start`/`pong`/`chunk`/`close`).
    #[allow(clippy::too_many_arguments)]
    async fn handle_open_stream_frame(
        state: &ServerOpenStreamState,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        notification: &JsonRpcNotification,
        sender_pubkey: &str,
        event_id: &str,
        is_encrypted: bool,
        is_gift_wrap: bool,
        outer_kind: u16,
        client_supports_open_stream: bool,
    ) {
        let token = notification
            .params
            .as_ref()
            .and_then(|p| p.get("progressToken"))
            .and_then(progress_token_string);
        // An active server→client writer owns this token's control frames.
        let writer = token
            .as_deref()
            .and_then(|t| state.event_id_for_token(t))
            .and_then(|eid| state.writer_for(&eid));

        match open_stream_frame_from_notification(notification) {
            Some(OpenStreamFrame::Ping { nonce }) => {
                if let Some(writer) = writer {
                    let _ = writer.pong(nonce).await;
                } else {
                    Self::feed_open_stream_reader(
                        state,
                        relay_pool,
                        encryption_mode,
                        gift_wrap_mode,
                        notification,
                        sender_pubkey,
                        event_id,
                        is_encrypted,
                        is_gift_wrap,
                        outer_kind,
                    )
                    .await;
                }
            }
            Some(OpenStreamFrame::Abort { reason }) => {
                if let Some(writer) = writer {
                    let _ = writer.abort(reason).await;
                } else {
                    Self::feed_open_stream_reader(
                        state,
                        relay_pool,
                        encryption_mode,
                        gift_wrap_mode,
                        notification,
                        sender_pubkey,
                        event_id,
                        is_encrypted,
                        is_gift_wrap,
                        outer_kind,
                    )
                    .await;
                }
            }
            Some(OpenStreamFrame::Start { .. }) => {
                Self::feed_open_stream_reader(
                    state,
                    relay_pool,
                    encryption_mode,
                    gift_wrap_mode,
                    notification,
                    sender_pubkey,
                    event_id,
                    is_encrypted,
                    is_gift_wrap,
                    outer_kind,
                )
                .await;
                // Stateless accept: only for clients that advertised support.
                if client_supports_open_stream {
                    if let Some(token) = token.as_deref() {
                        Self::publish_open_stream_control_frame(
                            state,
                            relay_pool,
                            encryption_mode,
                            gift_wrap_mode,
                            OpenStreamFrame::Accept,
                            token,
                            sender_pubkey,
                            Some(event_id),
                            is_encrypted,
                            is_gift_wrap,
                            outer_kind,
                        )
                        .await;
                    }
                }
            }
            // pong / chunk / close / accept → server-as-reader engine.
            _ => {
                Self::feed_open_stream_reader(
                    state,
                    relay_pool,
                    encryption_mode,
                    gift_wrap_mode,
                    notification,
                    sender_pubkey,
                    event_id,
                    is_encrypted,
                    is_gift_wrap,
                    outer_kind,
                )
                .await;
            }
        }
    }

    /// CEP-41 server-as-reader: feed an inbound frame to this peer's reader engine
    /// (created on demand) and publish a `pong` if its session asks for one.
    #[allow(clippy::too_many_arguments)]
    async fn feed_open_stream_reader(
        state: &ServerOpenStreamState,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        notification: &JsonRpcNotification,
        sender_pubkey: &str,
        event_id: &str,
        is_encrypted: bool,
        is_gift_wrap: bool,
        outer_kind: u16,
    ) {
        let outcome = {
            let mut store = state.receiver.lock().await;
            if !store.contains(sender_pubkey) {
                store.put(
                    sender_pubkey.to_string(),
                    OpenStreamReceiver::with_policy(state.policy),
                );
            }
            let receiver = store
                .get_mut(sender_pubkey)
                .expect("open-stream receiver present after insert");
            receiver.process_frame(notification).await
        };
        match outcome {
            Ok(FrameOutcome::SendPong(nonce)) => {
                if let Some(token) = notification
                    .params
                    .as_ref()
                    .and_then(|p| p.get("progressToken"))
                    .and_then(progress_token_string)
                {
                    Self::publish_open_stream_control_frame(
                        state,
                        relay_pool,
                        encryption_mode,
                        gift_wrap_mode,
                        OpenStreamFrame::Pong { nonce },
                        &token,
                        sender_pubkey,
                        Some(event_id),
                        is_encrypted,
                        is_gift_wrap,
                        outer_kind,
                    )
                    .await;
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    error = %error,
                    sender_pubkey = %sender_pubkey,
                    "Inbound open-stream frame rejected by server reader engine"
                );
            }
        }
    }

    /// CEP-41: publish one server→client control frame (`accept`/`pong`/`ping`) on
    /// the server-as-reader path, e-tagged to `correlated_event_id` and mirroring
    /// the inbound gift-wrap kind.
    #[allow(clippy::too_many_arguments)]
    async fn publish_open_stream_control_frame(
        state: &ServerOpenStreamState,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        frame: OpenStreamFrame,
        token: &str,
        recipient_pubkey: &str,
        correlated_event_id: Option<&str>,
        is_encrypted: bool,
        is_gift_wrap: bool,
        outer_kind: u16,
    ) {
        let recipient = match PublicKey::from_hex(recipient_pubkey) {
            Ok(pk) => pk,
            Err(_) => return,
        };
        let progress = state.next_control_progress();
        let notification = match frame.into_progress_notification(token, progress, None) {
            Ok(n) => n,
            Err(error) => {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    "Failed to build open-stream control frame"
                );
                return;
            }
        };
        let mut tags = BaseTransport::create_recipient_tags(&recipient);
        // The `e`-tag is present only when the frame correlates to a known request
        // event (accept/pong reply to an inbound frame); the keepalive ping for a
        // server-as-reader session has no correlation and is sent recipient-only.
        if let Some(eid) = correlated_event_id.and_then(|id| EventId::from_hex(id).ok()) {
            tags.push(Tag::event(eid));
        }
        let base = BaseTransport {
            relay_pool: Arc::clone(relay_pool),
            encryption_mode,
            is_connected: true,
        };
        let gift_wrap_kind = Self::select_outbound_gift_wrap_kind(
            gift_wrap_mode,
            is_encrypted,
            if is_gift_wrap { Some(outer_kind) } else { None },
        );
        if let Err(error) = base
            .send_mcp_message(
                &JsonRpcMessage::Notification(notification),
                &recipient,
                CTXVM_MESSAGES_KIND,
                tags,
                Some(is_encrypted),
                gift_wrap_kind,
            )
            .await
        {
            tracing::warn!(
                target: LOG_TARGET,
                error = %error,
                "Failed to publish open-stream control frame"
            );
        }
    }

    /// CEP-41: one keepalive sweep over the server-as-reader sessions (mirrors
    /// [`sweep_oversized_receivers`]). Drives each session's pure `tick`: idle →
    /// publish `ping`; probe/grace deadline → the reader aborted, so abort the
    /// paired writer too if one exists. Drops now-empty peer receivers.
    async fn sweep_open_stream_sessions(
        state: &ServerOpenStreamState,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
    ) {
        let now = Instant::now();
        let mut actions: Vec<(String, String, KeepaliveAction)> = Vec::new();
        {
            let mut store = state.receiver.lock().await;
            let mut empty_peers = Vec::new();
            for (peer, receiver) in store.iter_mut() {
                for (token, action) in receiver.registry_mut().tick_all(now) {
                    actions.push((peer.clone(), token, action));
                }
                if receiver.active_stream_count() == 0 {
                    empty_peers.push(peer.clone());
                }
            }
            for peer in empty_peers {
                store.pop(&peer);
            }
        }

        let probe_is_encrypted = encryption_mode != EncryptionMode::Disabled;
        for (peer, token, action) in actions {
            match action {
                KeepaliveAction::SendPing(nonce) => {
                    // Server-as-reader sessions have no `token_to_event` entry
                    // (that index is only populated for server→client writers), so
                    // this ping is uncorrelated until bidirectional streaming wires
                    // a reader-side event id through.
                    let correlated = state.event_id_for_token(&token);
                    Self::publish_open_stream_control_frame(
                        state,
                        relay_pool,
                        encryption_mode,
                        gift_wrap_mode,
                        OpenStreamFrame::Ping { nonce },
                        &token,
                        &peer,
                        correlated.as_deref(),
                        probe_is_encrypted,
                        false,
                        0,
                    )
                    .await;
                }
                KeepaliveAction::Abort(reason) => {
                    if let Some(eid) = state.event_id_for_token(&token) {
                        if let Some(writer) = state.writer_for(&eid) {
                            let _ = writer.abort(Some(reason)).await;
                        }
                    }
                }
                KeepaliveAction::None => {}
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
    use crate::relay::mock::MockRelayPool;
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

    #[tokio::test]
    async fn spawn_discoverability_publication_publishes_kind_0_and_10002_only() {
        let pool = Arc::new(MockRelayPool::new());
        let relay_pool: Arc<dyn RelayPoolTrait> = pool.clone();
        let config = NostrServerTransportConfig::default()
            .with_relay_urls(vec!["wss://relay.example.com".to_string()])
            .with_profile_metadata(ProfileMetadata::default().with_name("ffi-server"))
            .with_publish_relay_list(true);
        let mut transport = NostrServerTransport::with_relay_pool(config, relay_pool)
            .await
            .expect("transport should build");

        transport.spawn_discoverability_publication();
        for handle in transport.task_handles.drain(..) {
            handle.await.expect("discoverability task should not panic");
        }

        let events = pool.stored_events().await;
        assert!(
            events.iter().any(|e| e.kind == Kind::Custom(0)),
            "profile metadata should be published"
        );
        assert!(
            events
                .iter()
                .any(|e| e.kind == Kind::Custom(RELAY_LIST_METADATA_KIND)),
            "relay list should be published"
        );
        assert!(
            events
                .iter()
                .all(|e| e.kind != Kind::Custom(SERVER_ANNOUNCEMENT_KIND)),
            "direct discoverability publication must not emit CEP-6 announcements"
        );
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
    fn test_oversized_enabled_by_default() {
        let config = NostrServerTransportConfig::default();
        assert!(config.oversized_transfer.enabled);
    }

    #[test]
    fn test_oversized_support_tags_helper() {
        // Start from an explicit opt-out: the default is now enabled.
        let mut config = NostrServerTransportConfig::default().with_oversized_enabled(false);
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

    // ── CEP-41 open-stream capability advertisement ─────────────

    #[test]
    fn test_open_stream_support_tags_helper() {
        // Disabled (the default) → no tag; enabled → the single-element tag.
        assert!(open_stream_support_tags(&OpenStreamConfig::default()).is_empty());
        let names = first_tag_values(&open_stream_support_tags(&OpenStreamConfig::enabled()));
        assert_eq!(names, vec!["support_open_stream"]);
    }

    #[test]
    fn test_internal_common_capability_tags_merges_both() {
        let config = NostrServerTransportConfig::default()
            .with_oversized_enabled(true)
            .with_open_stream(OpenStreamConfig::enabled());
        let names = first_tag_values(&internal_common_capability_tags(&config));
        assert!(names.contains(&"support_oversized_transfer".to_string()));
        assert!(names.contains(&"support_open_stream".to_string()));
    }

    #[tokio::test]
    async fn test_announcement_includes_open_stream_tag_when_enabled() {
        let config = NostrServerTransportConfig {
            open_stream: OpenStreamConfig::enabled(),
            ..Default::default()
        };
        let pool: Arc<dyn RelayPoolTrait> = Arc::new(crate::relay::mock::MockRelayPool::new());
        let server = NostrServerTransport::with_relay_pool(config, pool)
            .await
            .expect("server transport construction");
        let names = first_tag_values(&server.announcement_manager.get_common_tags());
        assert!(
            names.contains(&"support_open_stream".to_string()),
            "announcement must advertise open-stream support when enabled"
        );
    }

    #[tokio::test]
    async fn test_announcement_omits_open_stream_tag_when_disabled() {
        // The default config has open-stream disabled (opt-in).
        let pool: Arc<dyn RelayPoolTrait> = Arc::new(crate::relay::mock::MockRelayPool::new());
        let server =
            NostrServerTransport::with_relay_pool(NostrServerTransportConfig::default(), pool)
                .await
                .expect("server transport construction");
        let names = first_tag_values(&server.announcement_manager.get_common_tags());
        assert!(!names.contains(&"support_open_stream".to_string()));
    }

    #[test]
    fn test_server_learns_client_open_stream_only_when_enabled() {
        let open_stream_tag = Tag::custom(
            TagKind::Custom(tags::SUPPORT_OPEN_STREAM.into()),
            Vec::<String>::new(),
        );
        let discovered = learn_peer_capabilities(&[open_stream_tag]);
        assert!(discovered.supports_open_stream);

        // Disabled server: the client flag is ignored.
        let mut session = ClientSession::new(false);
        let open_stream_enabled = false;
        session.supports_open_stream |= open_stream_enabled && discovered.supports_open_stream;
        assert!(!session.supports_open_stream);

        // Enabled server: the client flag is learned.
        let open_stream_enabled = true;
        session.supports_open_stream |= open_stream_enabled && discovered.supports_open_stream;
        assert!(session.supports_open_stream);
    }

    // ── CEP-41 response deferral (try_defer_open_stream_response) ───────

    /// A no-op writer (publishes nothing) for exercising the deferral decision.
    fn deferral_test_writer(token: &str) -> OpenStreamWriter {
        let publish_frame: PublishFrame = Arc::new(|_frame: JsonRpcNotification| {
            Box::pin(async move { Ok(EventId::all_zeros()) })
        });
        OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: token.to_string(),
            publish_frame,
            content_type: None,
            on_close: None,
            on_abort: None,
        })
    }

    /// Install a writer slot + `token → event_id` index entry, mirroring
    /// `create_open_stream_writer`.
    fn install_slot(
        state: &ServerOpenStreamState,
        event_id: &str,
        writer: OpenStreamWriter,
        terminated: bool,
    ) {
        let token = writer.progress_token().to_string();
        let snapshot = RouteSnapshot {
            client_pubkey: Keys::generate().public_key(),
            original_request_id: serde_json::json!(1),
            is_encrypted: false,
            mirrored_wrap_kind: None,
        };
        state.lock_slots().insert(
            event_id.to_string(),
            OpenStreamSlot {
                writer,
                snapshot,
                pending_response: None,
                terminated,
            },
        );
        state.lock_token_index().insert(token, event_id.to_string());
    }

    fn dummy_response() -> JsonRpcMessage {
        JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            result: serde_json::json!({ "ok": true }),
        })
    }

    #[tokio::test]
    async fn try_defer_open_stream_response_branch_coverage() {
        let config = NostrServerTransportConfig::default()
            .with_open_stream(OpenStreamConfig::default().with_enabled(true));
        let pool: Arc<dyn RelayPoolTrait> = Arc::new(MockRelayPool::new());
        let transport = NostrServerTransport::with_relay_pool(config, pool)
            .await
            .expect("server transport");

        // No slot for the event (`slots.get_mut` is None) → Passthrough.
        assert!(matches!(
            transport.try_defer_open_stream_response("evt-none", dummy_response()),
            OpenStreamDeferral::Passthrough(_)
        ));

        // `!writer.has_started()` — writer created (progressToken present) but the
        // tool never streamed → drop the writer and Passthrough. The slot AND the
        // token index entry must both be removed so the unused writer cannot leak.
        install_slot(
            &transport.open_stream,
            "evt-unstarted",
            deferral_test_writer("tok-unstarted"),
            false,
        );
        assert!(matches!(
            transport.try_defer_open_stream_response("evt-unstarted", dummy_response()),
            OpenStreamDeferral::Passthrough(_)
        ));
        assert!(
            transport
                .open_stream
                .lock_slots()
                .get("evt-unstarted")
                .is_none(),
            "unstarted writer slot must be removed (no leak)"
        );
        assert!(
            transport
                .open_stream
                .lock_token_index()
                .get("tok-unstarted")
                .is_none(),
            "unstarted writer token index must be removed (no leak)"
        );

        // `slot.terminated` (the function's "Ordering B") — started writer whose
        // stream already closed/aborted → deliver now from the snapshot (SendNow);
        // the slot + token index are freed.
        let terminal = deferral_test_writer("tok-terminal");
        terminal.start().await.expect("start");
        install_slot(&transport.open_stream, "evt-terminal", terminal, true);
        assert!(matches!(
            transport.try_defer_open_stream_response("evt-terminal", dummy_response()),
            OpenStreamDeferral::SendNow { .. }
        ));
        assert!(transport
            .open_stream
            .lock_slots()
            .get("evt-terminal")
            .is_none());
        assert!(transport
            .open_stream
            .lock_token_index()
            .get("tok-terminal")
            .is_none());

        // `else` of `slot.terminated` (the function's "Ordering A") — started
        // writer, stream still open → Deferred. The response is stashed and the
        // slot retained for the close/abort hook to flush.
        let open = deferral_test_writer("tok-open");
        open.start().await.expect("start");
        install_slot(&transport.open_stream, "evt-open", open, false);
        assert!(matches!(
            transport.try_defer_open_stream_response("evt-open", dummy_response()),
            OpenStreamDeferral::Deferred
        ));
        {
            let slots = transport.open_stream.lock_slots();
            let slot = slots.get("evt-open").expect("deferred slot retained");
            assert!(
                slot.pending_response.is_some(),
                "the deferred response must be stashed for the hook to flush"
            );
        }

        // Disabled gate — a server with open-stream disabled never exposes a
        // writer, so `send_response` never reaches the deferral decision at all.
        let disabled = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_open_stream(OpenStreamConfig::default().with_enabled(false)),
            Arc::new(MockRelayPool::new()) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("disabled server transport");
        install_slot(
            &disabled.open_stream,
            "evt-disabled",
            deferral_test_writer("tok-disabled"),
            false,
        );
        assert!(
            disabled.get_open_stream_writer("evt-disabled").is_none(),
            "a disabled server must not expose writers (deferral never attempted)"
        );
    }
}
