//! Server-side Nostr transport for ContextVM.
//!
//! Listens for incoming MCP requests from clients over Nostr, manages multi-client
//! sessions, handles request/response correlation, and optionally publishes
//! server announcements.

pub(crate) mod announcement_manager;
pub mod correlation_store;
pub mod middleware;
pub mod session_store;

pub use correlation_store::{RouteEntry, ServerEventRouteStore};
pub use middleware::{InboundContext, InboundMiddleware, Next};
pub use session_store::{SessionSnapshot, SessionStore};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use lru::LruCache;
use nostr_sdk::prelude::*;
use tokio_util::sync::CancellationToken;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::types::*;
use crate::core::validation;
use crate::encryption;
use crate::payments::constants::UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE;
use crate::payments::{PaymentInteractionPolicy, UnsupportedPaymentInteractionData};
use crate::relay::{RelayPool, RelayPoolTrait};
use crate::transport::base::BaseTransport;
use crate::transport::discovery_tags::{
    extract_payment_interaction, extract_pmis, learn_peer_capabilities,
};
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

    /// Composite key for the per-client writer-token index.
    ///
    /// The progress token is only unique *within* a peer — rmcp mints it from a
    /// per-peer counter (every client's first stream is `"0"`), so keying the
    /// writer index by token alone lets concurrent clients clobber each other's
    /// `token → event_id` entry and orphan a still-live writer (inbound pings
    /// then find no writer → client `Probe timeout` on an alive stream). Scope by
    /// `(client_pubkey, token)`, mirroring the TS `getProgressTokenKey` composite
    /// and the per-peer reader registry. `slots` (keyed by `event_id`, globally
    /// unique) is unaffected.
    fn client_token_key(client_pubkey_hex: &str, token: &str) -> String {
        format!("{client_pubkey_hex}:{token}")
    }

    /// Resolve `(client_pubkey, progress_token) → event_id`.
    fn event_id_for_token(&self, client_pubkey_hex: &str, token: &str) -> Option<String> {
        self.lock_token_index()
            .get(&Self::client_token_key(client_pubkey_hex, token))
            .cloned()
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

/// Tag sets a response can draw on, captured for the paths that have no `&self`.
///
/// The CEP-41 deferred publish is static so the writer's terminal hooks can call it, which means it
/// cannot reach the announcement manager. These are captured when the writer is created so a
/// deferred response composes the same tags the normal response path would.
#[derive(Clone)]
struct ResponseTagSources {
    /// First-response discovery tags, sent once per session.
    common: Vec<Tag>,
    /// CEP-8 `cap` pricing tags, appended to capability-list results.
    pricing: Vec<Tag>,
}

/// An injected targeted-response publish, for callers that have no `&self`.
///
/// The arguments are the recipient's public key in hex, the hex id of the request event to
/// correlate against, and the response to publish. Both strings are owned because the returned
/// future is `'static` and so cannot borrow the caller's buffers.
///
/// Cheaply clonable (`Arc`), so the transport can hand one to a middleware and keep handing out
/// more. Obtained from
/// [`targeted_response_sender`](NostrServerTransport::targeted_response_sender), which is also
/// where the construction-order rule is documented.
///
/// It looks the request's gift-wrap kind up itself, so it works for any caller. A middleware
/// already holds that value on its inbound context, so for that caller the lookup is redundant
/// and nothing depends on it.
pub type TargetedResponseSender = Arc<
    dyn Fn(String, String, JsonRpcMessage) -> BoxFuture<'static, crate::Result<()>> + Send + Sync,
>;

/// CEP-8: the outcome of one payment-interaction negotiation.
#[derive(Debug, PartialEq)]
enum NegotiationOutcome {
    /// Proceed to dispatch; the inbound context carries the session's effective mode.
    Proceed,
    /// The client requested `explicit_gating` on a request but the server does not support it:
    /// answer with `-32602` carrying this requested mode, and skip dispatch.
    RejectUnsupported {
        /// The mode the client asked for (always `explicit_gating`).
        requested: PaymentInteractionMode,
    },
}

/// CEP-8: write the effective mode and re-arm disclosure, but only when the mode actually changes.
///
/// A re-sent tag naming the mode already in force is idempotent: it must not re-arm disclosure.
fn apply_effective(
    session: &mut ClientSession,
    prev_effective: Option<PaymentInteractionMode>,
    next: PaymentInteractionMode,
) {
    if Some(next) != prev_effective {
        session.effective_payment_interaction = Some(next);
        session.has_disclosed_payment_interaction = false;
    }
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
    /// Ordered inbound middleware chain (FIFO). Registered before `start()`; moved
    /// into the event loop there. Empty means the direct forward path.
    inbound_middlewares: Vec<Arc<dyn InboundMiddleware>>,
    /// CEP-8: which payment-interaction lifecycles this server accepts. `None` means payments are
    /// not configured, which negotiates exactly like a transparent-only server (an
    /// `explicit_gating` request is rejected with a JSON-RPC `-32602`). Set before `start()`.
    supported_payment_interaction: Option<PaymentInteractionPolicy>,
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
    /// The inbound (client-signed) Nostr event, if this request carried one.
    ///
    /// `Some` for real client requests: the inner, signature-verified gift-wrap
    /// event for encrypted requests, the outer event for plaintext, or the
    /// carrying frame event for CEP-22 oversized reassembly. `None` only for
    /// requests the transport synthesizes itself (announcement /
    /// initialization drives), which carry no real Nostr event. Handlers reach
    /// it via [`InboundEvent`] in request
    /// `extensions`; see that type's docs.
    pub event: Option<Event>,
}

/// The Nostr public key (hex) of the client that issued the current request.
///
/// [`NostrServerWorker`](crate::rmcp_transport::NostrServerWorker) injects this
/// into every inbound request's rmcp `extensions` typemap, so a tool, resource,
/// or prompt handler can identify its caller:
///
/// ```ignore
/// use contextvm_sdk::transport::server::ClientPubkey;
/// use rmcp::service::RequestContext;
/// use rmcp::RoleServer;
///
/// let caller = ctx.extensions.get::<ClientPubkey>().map(|c| c.0.clone());
/// ```
///
/// The value is the raw hex pubkey (the same string as
/// [`IncomingRequest::client_pubkey`]). It is present on every real inbound
/// request, unlike `OpenStreamWriter`, which is injected only for open-stream
/// `tools/call` calls. This is purely a local affordance — it is never
/// serialized onto the wire. The inbound Nostr event id is available separately
/// as the rmcp request id (`ctx.id`), which the worker rewrites to the event id.
#[derive(Debug, Clone)]
pub struct ClientPubkey(pub String);

/// The inbound (client-signed) Nostr event for the current request.
///
/// Injected into a request's rmcp `extensions` typemap by
/// [`NostrServerWorker`](crate::rmcp_transport::NostrServerWorker) alongside
/// [`ClientPubkey`], so a tool, resource, or prompt handler can read the full
/// signed event — its `id`, `pubkey`, `sig`, … — to bind a tool call to the
/// publishing event, store it for later return, or audit it:
///
/// ```ignore
/// use contextvm_sdk::transport::server::InboundEvent;
/// use rmcp::service::RequestContext;
/// use rmcp::RoleServer;
///
/// if let Some(ev) = ctx.extensions.get::<InboundEvent>() {
///     let sig_hex = ev.0.sig.to_string(); // Schnorr signature, hex
///     // …
/// }
/// ```
///
/// For gift-wrapped requests this is the **inner**, signature-verified event
/// (the same one whose `pubkey` is surfaced as [`ClientPubkey`], so `ev.0.pubkey`
/// and `ClientPubkey` agree by construction). For plaintext requests it is the
/// outer request event. For CEP-22 oversized requests it is the carrying `end`
/// frame's event (a `notifications/progress` event signed by the client whose
/// `id` is the correlation id) — the request was chunked, so no single dedicated
/// request event exists. It is injected only for real client requests; synthetic
/// transport-internal requests (announcement / initialization drives) carry no
/// event, so `extensions.get::<InboundEvent>()` returns `None` for them. This is
/// purely a local affordance — it is never
/// serialized onto the wire. The event id is also reachable as the rmcp request
/// id (`ctx.id`); this type additionally exposes `sig`, which the server cannot
/// reconstruct without the client's private key.
#[derive(Debug, Clone)]
pub struct InboundEvent(pub Event);

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
            inbound_middlewares: Vec::new(),
            supported_payment_interaction: None,
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
            inbound_middlewares: Vec::new(),
            supported_payment_interaction: None,
            message_tx: Some(tx),
            message_rx: Some(rx),
            cancellation_token: CancellationToken::new(),
            task_handles: Vec::new(),
        })
    }

    /// Register an inbound middleware. Middleware runs in registration order (FIFO)
    /// as the final inbound stage before delivery to the MCP handler. Must be called
    /// before [`start`](Self::start); later registrations do not take effect.
    pub fn add_inbound_middleware(&mut self, middleware: Arc<dyn InboundMiddleware>) {
        debug_assert!(
            self.task_handles.is_empty(),
            "add_inbound_middleware must be called before start()"
        );
        self.inbound_middlewares.push(middleware);
    }

    /// CEP-8: set which payment-interaction lifecycles this server accepts.
    ///
    /// [`PaymentInteractionPolicy::Optional`] lets a client negotiate `explicit_gating`;
    /// [`PaymentInteractionPolicy::Transparent`] rejects such a request with a JSON-RPC `-32602`.
    /// Leaving this unset rejects it too, so a server that has not configured payments never
    /// accepts a gated lifecycle. (Note this is the opposite of
    /// [`PaymentInteractionPolicy::default`], which is the permissive `Optional`: that default
    /// applies when payments are being configured, not when they are absent.)
    ///
    /// Must be called before [`start`](Self::start): the policy is captured into the event loop
    /// there, so a later call does not take effect.
    ///
    /// The negotiated mode lives on the client's session, which is bounded and evicted LRU. A
    /// client whose session is evicted renegotiates from scratch, and per CEP-8 a client that sees
    /// no effective-mode disclosure must treat the negotiation as unestablished.
    ///
    /// # Invariant when advertising availability
    ///
    /// If you also advertise a `payment_interaction` tag through
    /// [`set_announcement_extra_tags`](Self::set_announcement_extra_tags), its value **must** equal
    /// the mode this policy can actually produce, or be absent. The advertisement is replayed onto
    /// a client's first response ahead of the effective-mode disclosure, the disclosure suppresses
    /// only an exact-value duplicate, and both SDKs read the first `payment_interaction` tag they
    /// find. A mismatched advertisement therefore wins on the wire and tells the client a mode the
    /// server is not running. In practice: advertise `explicit_gating` only under
    /// [`PaymentInteractionPolicy::Optional`].
    pub fn set_supported_payment_interaction(&mut self, policy: PaymentInteractionPolicy) {
        debug_assert!(
            self.task_handles.is_empty(),
            "set_supported_payment_interaction must be called before start()"
        );
        self.supported_payment_interaction = Some(policy);
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
        // The tag sets a deferred open-stream response needs, captured here because the publish
        // that sends it is static and cannot reach the announcement manager.
        let tag_sources = ResponseTagSources {
            common: self.announcement_manager.get_common_tags(),
            pricing: self.announcement_manager.get_pricing_tags().to_vec(),
        };
        let seen_gift_wrap_ids = self.seen_gift_wrap_ids.clone();
        let open_stream = self.open_stream.clone();
        let inbound_middlewares: Arc<[Arc<dyn InboundMiddleware>]> =
            Arc::from(std::mem::take(&mut self.inbound_middlewares));
        // CEP-8: reduce the policy to the single bit negotiation needs. Only an `optional` policy
        // lets a client hold `explicit_gating`; unset and `transparent` both reject it.
        let supports_explicit_gating = matches!(
            self.supported_payment_interaction,
            Some(PaymentInteractionPolicy::Optional)
        );
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
                inbound_middlewares,
                supports_explicit_gating,
                tag_sources,
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
        for (_, slot) in self.open_stream.lock_slots().drain() {
            slot.writer.dispose();
        }
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
        // CEP-8: decide the first-response effective-mode disclosure while the lock is held; the
        // tag itself is appended below, once the composed tag list exists.
        let disclose_effective = Self::take_payment_interaction_disclosure(session);
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
        // The whole composed list rides the CEP-22 start frame of an oversized response, and this
        // call sits above the published-size measurement below, so no tag the composer adds can
        // push a response over the fragmentation threshold unmeasured. The binding is deliberately
        // immutable: appending here after the measurement, or moving this call below it, is a
        // compile error rather than a silent ordering regression.
        let tags = Self::compose_response_tags(
            &base_tags,
            &discovery_tags,
            disclose_effective,
            self.announcement_manager.get_pricing_tags(),
            &response,
        );

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
            // CEP-22: the composed list rides the start frame; the continuation frames get
            // `base_tags`, so they carry only the routing tags.
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

    /// Publish a response to a specific client and request event **without consuming the
    /// request's correlation route**, so the request is not ended and a later
    /// [`send_response`](Self::send_response) for the same event still succeeds. That is what
    /// makes it usable by a transport-level gate that answers a request it does not own.
    ///
    /// Unlike [`send_response`](Self::send_response) it does not restore the original request
    /// id: the caller supplies the response with its id already set, which for a caller running
    /// ahead of the rmcp worker is the id the client sent.
    ///
    /// It composes the same tags every other server response carries, and therefore consumes
    /// the session's one-shot discovery-replay and effective-mode-disclosure latches like any
    /// other first response. Those latches are **not restored if the publish fails**, so a
    /// caller that retries gets a response carrying neither the discovery replay nor the
    /// disclosure. It also mirrors the request's gift-wrap kind (CEP-19).
    ///
    /// When the client has no session (an eviction race, say) this warns and returns `Ok(())`
    /// having published nothing.
    ///
    /// It does **not** fragment an over-threshold response (CEP-22). An `Err` is a malformed
    /// recipient or event id, a publish failure, or a response too large to send as one event; it
    /// is never a partial or abandoned fragmentation attempt, because this path never fragments.
    /// A malformed id is reported before the session is touched, so it costs neither latch.
    pub async fn send_targeted_response(
        &self,
        client_pubkey_hex: &str,
        event_id: &str,
        response: JsonRpcMessage,
    ) -> Result<()> {
        let mirrored_wrap_kind = self
            .request_wrap_kinds
            .read()
            .await
            .get(event_id)
            .copied()
            .flatten();
        Self::publish_targeted_response(
            &self.base,
            self.config.gift_wrap_mode,
            &self.sessions,
            &ResponseTagSources {
                common: self.announcement_manager.get_common_tags(),
                pricing: self.announcement_manager.get_pricing_tags().to_vec(),
            },
            client_pubkey_hex,
            event_id,
            mirrored_wrap_kind,
            response,
        )
        .await
    }

    /// The same publish as [`send_targeted_response`](Self::send_targeted_response), as an
    /// injectable closure for callers that have no `&self`, such as a middleware running on a
    /// detached task. Both forms delegate to one publish, so the tag-composition policy, the
    /// latch handling and the wrap-kind mirroring cannot diverge between them.
    ///
    /// Where the announcement tag sets come from does differ. The method reads them live on every
    /// call; this closure captures them once, here. A caller that sets those tags before building
    /// the sender gets identical output from both, which is the supported order; a caller that
    /// sets them afterwards gets tags from the method that the closure will never emit.
    ///
    /// **Call this after the announcement extra tags and pricing tags are set.** The returned
    /// sender captures those two tag sets at the moment it is built, and it is built earlier
    /// than [`start`](Self::start), so the weaker "set them before `start()`" rule the tag
    /// setters carry is not enough here: a caller that obeys only that rule still ships an empty
    /// discovery replay and empty pricing on every targeted response, which costs a stateless
    /// client the server's identity for the whole session and defeats the disclosure dedup.
    /// Nothing enforces the order at compile time, because the setters take `&mut self` and this
    /// closure holds owned clones.
    pub fn targeted_response_sender(&self) -> TargetedResponseSender {
        let relay_pool = Arc::clone(&self.base.relay_pool);
        let encryption_mode = self.base.encryption_mode;
        let gift_wrap_mode = self.config.gift_wrap_mode;
        let sessions = self.sessions.clone();
        let request_wrap_kinds = Arc::clone(&self.request_wrap_kinds);
        let tag_sources = ResponseTagSources {
            common: self.announcement_manager.get_common_tags(),
            pricing: self.announcement_manager.get_pricing_tags().to_vec(),
        };

        Arc::new(move |client_pubkey_hex, event_id, response| {
            let relay_pool = Arc::clone(&relay_pool);
            let sessions = sessions.clone();
            let request_wrap_kinds = Arc::clone(&request_wrap_kinds);
            let tag_sources = tag_sources.clone();
            Box::pin(async move {
                // The same local `BaseTransport` construction the other `&self`-less publishes
                // in this transport use.
                let base = BaseTransport {
                    relay_pool,
                    encryption_mode,
                    is_connected: true,
                };
                let mirrored_wrap_kind = request_wrap_kinds
                    .read()
                    .await
                    .get(&event_id)
                    .copied()
                    .flatten();
                Self::publish_targeted_response(
                    &base,
                    gift_wrap_mode,
                    &sessions,
                    &tag_sources,
                    &client_pubkey_hex,
                    &event_id,
                    mirrored_wrap_kind,
                    response,
                )
                .await
            })
        })
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
            let client = slot.snapshot.client_pubkey.to_hex();
            slots.remove(event_id);
            drop(slots);
            self.open_stream
                .lock_token_index()
                .remove(&ServerOpenStreamState::client_token_key(&client, &token));
            return OpenStreamDeferral::Passthrough(response);
        }

        if slot.terminated {
            // Ordering B (the common case): the stream already closed/aborted —
            // deliver now from the captured snapshot (the route may be swept).
            let snapshot = slot.snapshot.clone();
            let token = slot.writer.progress_token().to_string();
            let client = snapshot.client_pubkey.to_hex();
            slots.remove(event_id);
            drop(slots);
            self.open_stream
                .lock_token_index()
                .remove(&ServerOpenStreamState::client_token_key(&client, &token));
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
            &self.sessions,
            &ResponseTagSources {
                common: self.announcement_manager.get_common_tags(),
                pricing: self.announcement_manager.get_pricing_tags().to_vec(),
            },
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
        sessions: &SessionStore,
        tag_sources: &ResponseTagSources,
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
        let base_tags =
            BaseTransport::create_response_tags(&snapshot.client_pubkey, &event_id_parsed);

        // A deferred response is still a server-to-client response, so it carries the same
        // first-response tags the normal path sends: the CEP-35 discovery set and the CEP-8
        // effective-mode disclosure. Both one-shot latches are read and flipped under a single
        // session lock, and the tags are appended after it drops. When the session is gone, both
        // latches are left untouched so a later response still carries them.
        let (send_discovery, disclose_effective) = {
            let mut sessions_w = sessions.write().await;
            match sessions_w.get_mut(&snapshot.client_pubkey.to_hex()) {
                Some(session) => {
                    let send_discovery = !session.has_sent_common_tags;
                    session.has_sent_common_tags = true;
                    (
                        send_discovery,
                        Self::take_payment_interaction_disclosure(session),
                    )
                }
                None => (false, None),
            }
        };
        // The discovery set can carry the server's availability advertisement, and a streaming tool
        // is free to return a capability list, so this path composes through the same policy the
        // normal response path does rather than restating it.
        let discovery: &[Tag] = if send_discovery {
            &tag_sources.common
        } else {
            &[]
        };
        let tags = Self::compose_response_tags(
            &base_tags,
            discovery,
            disclose_effective,
            &tag_sources.pricing,
            &response,
        );

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

    /// The targeted-response publish itself, shared by the `&self` method and the injected
    /// closure so the two forms cannot drift apart. Static for the same reason the deferred
    /// publish is: a detached caller has no `self` in scope.
    ///
    /// The caller supplies the recipient and the request event to correlate against, so this
    /// never touches the correlation route.
    // Eight parameters is what a `&self`-less publish needs: the two transport handles, the
    // session store, the captured tag sources, the two routing values, the mirrored wrap kind,
    // and the payload.
    #[allow(clippy::too_many_arguments)]
    async fn publish_targeted_response(
        base: &BaseTransport,
        gift_wrap_mode: GiftWrapMode,
        sessions: &SessionStore,
        tag_sources: &ResponseTagSources,
        client_pubkey_hex: &str,
        event_id: &str,
        mirrored_wrap_kind: Option<u16>,
        response: JsonRpcMessage,
    ) -> Result<()> {
        // Both routing values are validated before the session is touched. Unlike the normal
        // response path, which reads them back out of its own correlation store, these arrive
        // from an external caller and are therefore reachable bad input; parsing first keeps a
        // malformed argument from consuming the session's one-shot latches for a response that
        // is never published.
        let client_pubkey = PublicKey::from_hex(client_pubkey_hex).map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                client_pubkey = %client_pubkey_hex,
                "Invalid client pubkey for targeted response"
            );
            Error::Other(error.to_string())
        })?;
        let event_id_parsed = EventId::from_hex(event_id).map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                event_id = %event_id,
                "Invalid event id for targeted response"
            );
            Error::Other(error.to_string())
        })?;

        // Answering a client that has since been evicted costs one read and publishes nothing.
        // Both one-shot latches are read and flipped under a single guard, and the tags are
        // composed after it drops.
        let resolved = {
            let mut sessions_w = sessions.write().await;
            sessions_w.get_mut(client_pubkey_hex).map(|session| {
                let send_discovery = !session.has_sent_common_tags;
                session.has_sent_common_tags = true;
                (
                    session.is_encrypted,
                    send_discovery,
                    Self::take_payment_interaction_disclosure(session),
                )
            })
        };
        let Some((is_encrypted, send_discovery, disclose_effective)) = resolved else {
            tracing::warn!(
                target: LOG_TARGET,
                client_pubkey = %client_pubkey_hex,
                event_id = %event_id,
                "No session for targeted response; dropping it"
            );
            return Ok(());
        };

        let base_tags = BaseTransport::create_response_tags(&client_pubkey, &event_id_parsed);
        let discovery: &[Tag] = if send_discovery {
            &tag_sources.common
        } else {
            &[]
        };
        let tags = Self::compose_response_tags(
            &base_tags,
            discovery,
            disclose_effective,
            &tag_sources.pricing,
            &response,
        );

        let gift_wrap_kind =
            Self::select_outbound_gift_wrap_kind(gift_wrap_mode, is_encrypted, mirrored_wrap_kind);
        base.send_mcp_message(
            &response,
            &client_pubkey,
            CTXVM_MESSAGES_KIND,
            tags,
            Some(is_encrypted),
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
        sessions: &SessionStore,
        tag_sources: &ResponseTagSources,
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
        state
            .lock_token_index()
            .remove(&ServerOpenStreamState::client_token_key(
                &snapshot.client_pubkey.to_hex(),
                &token,
            ));
        if let Err(error) = Self::publish_open_stream_deferred_response(
            base,
            gift_wrap_mode,
            event_id,
            &snapshot,
            response,
            sessions,
            tag_sources,
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
    ///
    /// Announcements and the normal response path read these live; the CEP-41 open-stream writer
    /// hooks capture the first-response tag set once, when [`start`](Self::start) runs. That split
    /// falls through the middle of a single deferred delivery rather than between the deferred and
    /// normal paths: if the stream ends before the final response arrives, `send_response` delivers
    /// it and reads these live, but if the response is stashed while the stream is still open, the
    /// close or abort hook flushes it from the `start()` snapshot. Which of the two happens is a
    /// race, so after a post-`start()` change the same response can go out with either set. Set
    /// these before `start()` if a client's very first response may be a deferred one.
    pub fn set_announcement_extra_tags(&mut self, tags: Vec<Tag>) {
        self.announcement_manager.set_extra_common_tags(tags);
    }

    /// Sets pricing tags to include in announcement/list events and capability list responses.
    ///
    /// Announcements and the normal response path read these live; the CEP-41 open-stream writer
    /// hooks capture them when [`start`](Self::start) runs, with the same race between the two
    /// deferred-delivery orderings described on
    /// [`set_announcement_extra_tags`](Self::set_announcement_extra_tags). A deferred response
    /// flushed by a stream's close or abort hook carries the prices as they stood at `start()`,
    /// while one delivered by `send_response` after the stream has already ended carries them live.
    /// Set these before `start()` if a streaming tool can return a capability-list result.
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

    /// Compose an outbound response's tags in the canonical order: routing, then the CEP-35
    /// discovery replay, then the CEP-8 effective-mode disclosure, then CEP-8 `cap` pricing.
    ///
    /// Every server response path composes its tags here, so the four steps and their order are
    /// written down once and every path inherits them.
    ///
    /// Pure and synchronous: it takes values the caller has already resolved and holds
    /// no lock and performs no I/O. Each caller reads its session's one-shot discovery and
    /// disclosure latches under its own session guard and drops that guard before calling this, so
    /// no lock is ever held across the publish that follows.
    ///
    /// `disclose` is the effective mode to advertise, or `None` when the obligation is already
    /// discharged. A disclosure is skipped when the discovery set already carries the server's
    /// availability advertisement for the same mode, which satisfies it on its own.
    ///
    /// When the advertisement names a *different* mode the dedup does not fire, so the event
    /// carries two `payment_interaction` tags and the **advertisement wins**, because it is
    /// ordered first and both SDKs' readers take the first tag. That combination is a server
    /// misconfiguration rather than a protocol state, so it is logged here rather than repaired:
    /// the availability advertisement must either name the mode the session negotiated or be
    /// absent.
    ///
    /// The `cap` gate is shape-based rather than method-based, because the request method is gone
    /// by response time: pricing rides a result that looks like a capability list, and therefore
    /// never rides an error response.
    fn compose_response_tags(
        base_tags: &[Tag],
        discovery: &[Tag],
        disclose: Option<PaymentInteractionMode>,
        pricing: &[Tag],
        response: &JsonRpcMessage,
    ) -> Vec<Tag> {
        let mut tags = BaseTransport::compose_outbound_tags(base_tags, discovery, &[]);

        // CEP-8: disclose the effective mode. The server's availability advertisement can already
        // be riding this same first response with the same value, in which case it satisfies the
        // disclosure on its own and a second tag would be redundant.
        if let Some(effective) = disclose {
            // The comparison is derived from the builder, not a second hand-written mapping, so the
            // two can never drift apart. Name and value only, matching how both SDKs read the tag.
            let disclosure = crate::payments::tags::payment_interaction_tag(effective);
            let expected = disclosure.clone().to_vec();
            let already_present = tags.iter().any(|tag| {
                let parts = tag.clone().to_vec();
                parts.first() == expected.first() && parts.get(1) == expected.get(1)
            });
            if !already_present {
                // A same-name tag that survived the dedup carries a different value, so the
                // advertisement and the disclosure disagree and the reader takes the
                // advertisement. Warn rather than rewrite: the tag list stays exactly what every
                // other response path produces, and the fix belongs to whatever set the
                // advertisement.
                if let Some(conflicting) = tags.iter().find_map(|tag| {
                    let parts = tag.clone().to_vec();
                    match (parts.first(), parts.get(1)) {
                        (Some(name), Some(value)) if Some(name) == expected.first() => {
                            Some(value.clone())
                        }
                        _ => None,
                    }
                }) {
                    tracing::warn!(
                        target: LOG_TARGET,
                        advertised = %conflicting,
                        effective = ?effective,
                        "Server availability advertisement disagrees with the session's effective \
                         payment interaction mode; the client will read the advertised value"
                    );
                }
                tags.push(disclosure);
            }
        }

        // CEP-8: attach the pricing tags to capability-list responses so clients can read prices
        // without waiting for a payment_required error.
        if let JsonRpcMessage::Response(ref r) = response {
            if Self::is_capability_list_result(&r.result) {
                tags.extend(pricing.iter().cloned());
            }
        }

        tags
    }

    // ── CEP-8 payment-interaction negotiation ─────────────────────

    /// Negotiate this session's payment-interaction mode from an inbound message's inner tags.
    ///
    /// Pure and synchronous: it mutates the session's three payment fields and reports whether the
    /// caller must answer with `-32602` instead of dispatching. `supports_explicit_gating` is the
    /// server policy reduced to one bit.
    fn negotiate_payment_interaction(
        session: &mut ClientSession,
        inner_tags: &[Tag],
        method: Option<&str>,
        is_request: bool,
        supports_explicit_gating: bool,
    ) -> NegotiationOutcome {
        // A fresh `initialize` request re-opens negotiation for a reconnecting stateful client.
        // Only the payment fields reset; the learned transport capabilities stay as they are.
        if is_request && method == Some("initialize") {
            session.requested_payment_interaction = None;
            session.effective_payment_interaction = None;
            session.has_disclosed_payment_interaction = false;
        }

        // The mode is upserted only by a message that carries the tag; an absent tag inherits the
        // mode currently in force.
        let Some(raw) = extract_payment_interaction(inner_tags) else {
            return NegotiationOutcome::Proceed;
        };
        let prev_effective = session.effective_payment_interaction;

        match raw.as_str() {
            "transparent" | "explicit_gating" => {
                let mode = if raw == "explicit_gating" {
                    PaymentInteractionMode::ExplicitGating
                } else {
                    PaymentInteractionMode::Transparent
                };
                session.requested_payment_interaction = Some(mode);

                if mode == PaymentInteractionMode::ExplicitGating && !supports_explicit_gating {
                    // CEP-8 says a rejected upsert leaves the effective mode unchanged. Writing
                    // `transparent` here honors that: the policy is fixed for the transport's
                    // lifetime, so a server that rejects gating can never have held any other
                    // effective mode, and an unset mode already means `transparent`.
                    apply_effective(session, prev_effective, PaymentInteractionMode::Transparent);

                    // A request gets an error; a notification silently keeps the downgrade.
                    if is_request {
                        return NegotiationOutcome::RejectUnsupported { requested: mode };
                    }
                }

                let next = if supports_explicit_gating {
                    mode
                } else {
                    PaymentInteractionMode::Transparent
                };
                apply_effective(session, prev_effective, next);
                NegotiationOutcome::Proceed
            }
            // An unrecognized value is treated as a request for the default mode.
            _ => {
                session.requested_payment_interaction = Some(PaymentInteractionMode::Transparent);
                apply_effective(session, prev_effective, PaymentInteractionMode::Transparent);
                NegotiationOutcome::Proceed
            }
        }
    }

    /// Build the CEP-8 `-32602` unsupported-`payment_interaction` error for an inbound message.
    ///
    /// The id is the message's **original inner request id**, never the Nostr event id (the worker's
    /// id rewrite has not run at this point), so the client correlates it to the request it sent.
    fn unsupported_payment_interaction_error(
        inbound: &JsonRpcMessage,
        requested: PaymentInteractionMode,
    ) -> JsonRpcMessage {
        let id = match inbound {
            JsonRpcMessage::Request(req) => req.id.clone(),
            // Unreachable: negotiation only rejects requests.
            _ => serde_json::Value::Null,
        };
        JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id,
            error: JsonRpcError {
                code: UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE,
                message: "Unsupported payment_interaction mode: explicit_gating".to_string(),
                // Serializing this struct cannot fail; `.ok()` only avoids a `?` the event loop's
                // `()` return type cannot carry.
                data: serde_json::to_value(UnsupportedPaymentInteractionData::new(requested)).ok(),
            },
        })
    }

    /// CEP-8: the effective mode to disclose on this response, flipping the session's latch.
    ///
    /// Fires only when the client requested a non-transparent mode, and only once per negotiated
    /// mode. The latch flips even when the caller ends up deduplicating the tag away, because an
    /// already-present tag of the same value satisfies the disclosure obligation.
    fn take_payment_interaction_disclosure(
        session: &mut ClientSession,
    ) -> Option<PaymentInteractionMode> {
        match (
            session.requested_payment_interaction,
            session.effective_payment_interaction,
        ) {
            (Some(requested), Some(effective))
                if requested != PaymentInteractionMode::Transparent
                    && !session.has_disclosed_payment_interaction =>
            {
                session.has_disclosed_payment_interaction = true;
                Some(effective)
            }
            _ => None,
        }
    }

    /// True when a JSON-RPC result is an MCP capability-list result, which is what CEP-8 attaches
    /// `cap` pricing tags to.
    ///
    /// Shape-based rather than method-based: the request method is no longer available by response
    /// time. Mirrors the ts-sdk gate, which parses the result against the four list schemas; those
    /// schemas are permissive about unknown keys, so this is bare key presence with no exclusions
    /// (a result carrying both `content` and a list array is a list result in both SDKs).
    ///
    /// The one looseness is that ts also validates the array elements, for all four keys, while this
    /// checks only that the value is an array. A malformed list therefore gets `cap` tags here and
    /// not in ts. That is harmless: `cap` is a reference signal, never authoritative for the amount
    /// charged, and no real MCP handler emits that shape.
    fn is_capability_list_result(result: &serde_json::Value) -> bool {
        ["tools", "resources", "resourceTemplates", "prompts"]
            .iter()
            .any(|key| result.get(key).is_some_and(serde_json::Value::is_array))
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
        middlewares: Arc<[Arc<dyn InboundMiddleware>]>,
        supports_explicit_gating: bool,
        tag_sources: ResponseTagSources,
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
                        &sessions,
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

                let (content, sender_pubkey, event_id, is_encrypted, inner_tags, inbound_event) =
                    if is_gift_wrap {
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
                                        let inbound = inner.clone();
                                        let inner_tags: Vec<Tag> = inner.tags.to_vec();
                                        (
                                            inner.content,
                                            inner.pubkey.to_hex(),
                                            inner.id.to_hex(),
                                            true,
                                            inner_tags,
                                            Some(inbound),
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
                        // Verify the signature (and that `id` matches content) before
                        // trusting `pubkey`/`id` for handler identity + correlation.
                        // Redundant against the default RelayPool (it verifies inbound
                        // signatures), but keeps caller identity independent of the
                        // pool impl — a custom RelayPoolTrait may skip verification.
                        if let Err(e) = event.verify() {
                            tracing::warn!(
                                target: LOG_TARGET,
                                "Plaintext event signature verification failed: {e}"
                            );
                            continue;
                        }
                        let inbound = (*event).clone();
                        (
                            event.content.clone(),
                            event.pubkey.to_hex(),
                            event.id.to_hex(),
                            false,
                            event.tags.to_vec(),
                            Some(inbound),
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

                // CEP-8: negotiate this session's payment-interaction mode from the inner tags.
                let is_request = matches!(mcp_msg, JsonRpcMessage::Request(_));
                let inbound_method = mcp_msg.method().map(str::to_owned);
                let negotiation = Self::negotiate_payment_interaction(
                    session,
                    &inner_tags,
                    inbound_method.as_deref(),
                    is_request,
                    supports_explicit_gating,
                );
                // Capture both context inputs as owned locals while the lock is held, so every
                // branch below can dispatch after dropping it.
                let client_pmis = extract_pmis(&inner_tags);
                let client_pmis = (!client_pmis.is_empty()).then_some(client_pmis);
                // A resolved mode, not the raw option: an unset effective mode means `transparent`,
                // and a middleware comparing against a concrete mode must not see `None`.
                let effective_payment_interaction = Some(
                    session
                        .effective_payment_interaction
                        .unwrap_or(PaymentInteractionMode::Transparent),
                );

                // CEP-8: the client asked for a lifecycle this server does not offer. Answer the
                // request with `-32602` and do not dispatch it.
                if let NegotiationOutcome::RejectUnsupported { requested } = negotiation {
                    let error_response =
                        Self::unsupported_payment_interaction_error(&mcp_msg, requested);
                    drop(sessions_w);
                    if let Ok(client_pk) = PublicKey::from_hex(&sender_pubkey) {
                        let event_id_parsed =
                            EventId::from_hex(&event_id).unwrap_or(EventId::all_zeros());
                        let tags =
                            BaseTransport::create_response_tags(&client_pk, &event_id_parsed);
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
                                "Failed to send unsupported payment_interaction error response"
                            );
                        }
                    }
                    continue;
                }

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
                                inbound_event,
                                &middlewares,
                                client_pmis,
                                effective_payment_interaction,
                                &sessions,
                                &tag_sources,
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
                                &sessions,
                                &tag_sources,
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

                // Forward to consumer (through the inbound middleware chain).
                middleware::dispatch_inbound(
                    &middlewares,
                    &tx,
                    &event_routes,
                    mcp_msg,
                    sender_pubkey,
                    event_id,
                    is_encrypted,
                    if is_gift_wrap { Some(outer_kind) } else { None },
                    client_pmis,
                    effective_payment_interaction,
                    inbound_event,
                );
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
        inbound_event: Option<Event>,
        chain: &Arc<[Arc<dyn InboundMiddleware>]>,
        // CEP-8: negotiated for the carrying frame's iteration and forwarded onto the reassembled
        // request. Transfer frames are notifications, so an oversized request carrying an
        // unsupported mode is downgraded and disclosed on the response rather than drawing a
        // `-32602`, which is what the reference implementation does as well.
        client_pmis: Option<Vec<String>>,
        payment_interaction: Option<PaymentInteractionMode>,
        sessions: &SessionStore,
        tag_sources: &ResponseTagSources,
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
                            sessions,
                            tag_sources,
                        );
                    }
                }
                middleware::dispatch_inbound(
                    chain,
                    tx,
                    event_routes,
                    message,
                    sender_pubkey.to_string(),
                    event_id.to_string(),
                    is_encrypted,
                    if is_gift_wrap { Some(outer_kind) } else { None },
                    client_pmis,
                    payment_interaction,
                    inbound_event,
                );
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
        sessions: &SessionStore,
        tag_sources: &ResponseTagSources,
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

        // Terminal hooks flush any deferred final response from the snapshot. They carry a session
        // handle and the first-response tag set so a flushed response still gets the same discovery
        // tags and effective-mode disclosure the normal response path sends.
        let on_close: OnCloseHook = {
            let state = state.clone();
            let relay_pool = Arc::clone(relay_pool);
            let event_id = event_id.to_string();
            let sessions = sessions.clone();
            let tag_sources = tag_sources.clone();
            Arc::new(move || {
                let state = state.clone();
                let relay_pool = Arc::clone(&relay_pool);
                let event_id = event_id.clone();
                let sessions = sessions.clone();
                let tag_sources = tag_sources.clone();
                Box::pin(async move {
                    let base = BaseTransport {
                        relay_pool,
                        encryption_mode,
                        is_connected: true,
                    };
                    Self::flush_open_stream_response(
                        &state,
                        &base,
                        gift_wrap_mode,
                        &event_id,
                        &sessions,
                        &tag_sources,
                    )
                    .await;
                })
            })
        };
        let on_abort: OnAbortHook = {
            let state = state.clone();
            let relay_pool = Arc::clone(relay_pool);
            let event_id = event_id.to_string();
            let sessions = sessions.clone();
            let tag_sources = tag_sources.clone();
            Arc::new(move |_reason| {
                let state = state.clone();
                let relay_pool = Arc::clone(&relay_pool);
                let event_id = event_id.clone();
                let sessions = sessions.clone();
                let tag_sources = tag_sources.clone();
                Box::pin(async move {
                    let base = BaseTransport {
                        relay_pool,
                        encryption_mode,
                        is_connected: true,
                    };
                    Self::flush_open_stream_response(
                        &state,
                        &base,
                        gift_wrap_mode,
                        &event_id,
                        &sessions,
                        &tag_sources,
                    )
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
            // CEP-41 sender-side keepalive: reuse the reader idle/probe knobs (one
            // idle/probe pair per stream, per spec). `None` when idle is disabled,
            // keeping the writer keepalive in lockstep with the sweep gate
            // (`open_stream_sweep_enabled` is false when `idle_timeout_ms == 0`).
            idle_timeout: (state.policy.idle_timeout_ms != 0)
                .then(|| Duration::from_millis(state.policy.idle_timeout_ms)),
            probe_timeout: Duration::from_millis(state.policy.probe_timeout_ms),
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
        state.lock_token_index().insert(
            ServerOpenStreamState::client_token_key(client_pubkey_hex, progress_token),
            event_id.to_string(),
        );
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
            .and_then(|t| state.event_id_for_token(sender_pubkey, t))
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
            Some(OpenStreamFrame::Pong { nonce }) => {
                // CEP-41: a `pong` for a stream the server is *writing* (server→client)
                // acknowledges our keepalive probe (completing the round trip the reader
                // sweep opened). Only intercept when a writer owns the token;
                // client→server streams fall through to the reader engine below.
                if let Some(writer) = writer {
                    writer.ack_probe(&nonce);
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
        sessions: &SessionStore,
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
                    let correlated = state.event_id_for_token(&peer, &token);
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
                    if let Some(eid) = state.event_id_for_token(&peer, &token) {
                        if let Some(writer) = state.writer_for(&eid) {
                            let _ = writer.abort(Some(reason)).await;
                        }
                    }
                }
                KeepaliveAction::None => {}
            }
        }

        // CEP-41: writer (server→client) keepalive — each producer stream MUST
        // maintain an idle timeout (CEP-41 §Timeout and Keepalive). A client that
        // vanishes without `abort` is probed here and aborted on a missing `pong`;
        // the writer's `on_abort` hook then flushes any deferred final response.
        // This is the mirror of the reader sweep above for sender-side streams.
        // The client pubkey is captured up front: the slot may be removed by the
        // time we act on an `Abort` (the writer's on_abort flush drops it), and we
        // need it to release the dead client's session.
        let writer_actions: Vec<(String, PublicKey, KeepaliveAction)> = state
            .lock_slots()
            .iter()
            .map(|(event_id, slot)| {
                (
                    event_id.clone(),
                    slot.snapshot.client_pubkey,
                    slot.writer.tick(now),
                )
            })
            .filter(|(.., action)| !matches!(action, KeepaliveAction::None))
            .collect();
        for (event_id, client_pubkey, action) in writer_actions {
            let Some(writer) = state.writer_for(&event_id) else {
                continue;
            };
            match action {
                KeepaliveAction::SendPing(nonce) => {
                    if let Err(error) = writer.send_probe(nonce).await {
                        tracing::warn!(
                            target: LOG_TARGET,
                            error = %error,
                            event_id = %event_id,
                            "Failed to publish open-stream keepalive ping"
                        );
                    }
                }
                KeepaliveAction::Abort(reason) => {
                    if let Err(error) = writer.abort(Some(reason)).await {
                        tracing::warn!(
                            target: LOG_TARGET,
                            error = %error,
                            event_id = %event_id,
                            "Failed to abort open-stream writer on probe timeout"
                        );
                    }
                    // CEP-41: a probe timeout means the client is gone — release
                    // its session too ("release local state"), mirroring the TS
                    // `handleProbeTimeout`. Done after the abort so the abort frame
                    // is published first; eviction is independent of the
                    // snapshot-backed deferred-response flush. Only the sweep
                    // produces a writer `Abort`, so this path is probe-timeout only
                    // (a tool-initiated `close`/`abort` does not evict the session).
                    let pubkey_hex = client_pubkey.to_hex();
                    if sessions.remove_session(&pubkey_hex).await {
                        if let Some(cb) = sessions.eviction_callback() {
                            cb(pubkey_hex);
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
            idle_timeout: None,
            probe_timeout: Duration::from_millis(20_000),
        })
    }

    /// Install a writer slot + `token → event_id` index entry, mirroring
    /// `create_open_stream_writer`.
    fn install_slot(
        state: &ServerOpenStreamState,
        event_id: &str,
        writer: OpenStreamWriter,
        terminated: bool,
    ) -> String {
        let token = writer.progress_token().to_string();
        let client_pubkey = Keys::generate().public_key();
        let client_pubkey_hex = client_pubkey.to_hex();
        let snapshot = RouteSnapshot {
            client_pubkey,
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
        state.lock_token_index().insert(
            ServerOpenStreamState::client_token_key(&client_pubkey_hex, &token),
            event_id.to_string(),
        );
        client_pubkey_hex
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
        let unstarted_pk = install_slot(
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
                .get(&ServerOpenStreamState::client_token_key(
                    &unstarted_pk,
                    "tok-unstarted"
                ))
                .is_none(),
            "unstarted writer token index must be removed (no leak)"
        );

        // `slot.terminated` (the function's "Ordering B") — started writer whose
        // stream already closed/aborted → deliver now from the snapshot (SendNow);
        // the slot + token index are freed.
        let terminal = deferral_test_writer("tok-terminal");
        terminal.start().await.expect("start");
        let terminal_pk = install_slot(&transport.open_stream, "evt-terminal", terminal, true);
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
            .get(&ServerOpenStreamState::client_token_key(
                &terminal_pk,
                "tok-terminal"
            ))
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

    #[tokio::test]
    async fn open_stream_writer_token_index_is_scoped_per_client() {
        // Regression for the multi-client collision: the writer `token → event_id`
        // index is keyed by `(client_pubkey, token)`, so two clients whose first
        // stream both carry token "0" (rmcp's per-peer counter starts at 0) must
        // NOT clobber each other. Each routes to its own writer, and either's
        // cleanup leaves the other's live writer routable — otherwise an inbound
        // ping finds no writer and the peer `Probe timeout`s an alive stream.
        let config = NostrServerTransportConfig::default()
            .with_open_stream(OpenStreamConfig::default().with_enabled(true));
        let pool: Arc<dyn RelayPoolTrait> = Arc::new(MockRelayPool::new());
        let transport = NostrServerTransport::with_relay_pool(config, pool)
            .await
            .expect("server transport");
        let state = &transport.open_stream;

        // Two clients both open their first stream: token "0" twice, distinct keys.
        let pk_a = install_slot(state, "evtA", deferral_test_writer("0"), false);
        let pk_b = install_slot(state, "evtB", deferral_test_writer("0"), false);

        // Both writers are live and each token routes to its OWN event — no clobber.
        assert_ne!(pk_a, pk_b, "distinct clients");
        assert!(state.writer_for("evtA").is_some());
        assert!(state.writer_for("evtB").is_some());
        assert_eq!(
            state.event_id_for_token(&pk_a, "0"),
            Some("evtA".to_string())
        );
        assert_eq!(
            state.event_id_for_token(&pk_b, "0"),
            Some("evtB".to_string())
        );

        // B's cleanup removes only B's key; A's writer stays routable (the exact
        // condition the bare-token index violated — A's ping would find nothing).
        state
            .lock_token_index()
            .remove(&ServerOpenStreamState::client_token_key(&pk_b, "0"));
        assert_eq!(
            state.event_id_for_token(&pk_a, "0"),
            Some("evtA".to_string())
        );
        assert!(state.writer_for("evtA").is_some());
        assert_eq!(state.event_id_for_token(&pk_b, "0"), None);
    }

    // ── CEP-41 writer keepalive sweep (server→client, silent-client abort) ──

    /// A keepalive-armed writer whose publish closure is a no-op. The sweep
    /// drives `tick`; a missing `pong` aborts it on the probe deadline.
    fn keepalive_test_writer(token: &str, idle_ms: u64, probe_ms: u64) -> OpenStreamWriter {
        let publish_frame: PublishFrame = Arc::new(|_frame: JsonRpcNotification| {
            Box::pin(async move { Ok(EventId::all_zeros()) })
        });
        OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: token.to_string(),
            publish_frame,
            content_type: None,
            on_close: None,
            on_abort: None,
            idle_timeout: Some(Duration::from_millis(idle_ms)),
            probe_timeout: Duration::from_millis(probe_ms),
        })
    }

    /// Regression: a client that silently disappears (sends no `pong`, no
    /// `abort`) is detected by the server-writer keepalive sweep — the writer is
    /// probed after the idle window and aborted once the probe times out, and the
    /// dead client's session is evicted (CEP-41 "release local state", mirroring
    /// the TS `handleProbeTimeout`). This is the rs-sdk port of the TS 0.13.8 fix;
    /// without it the writer (and any upstream producer keyed on `is_active`)
    /// leaks indefinitely. `tokio::time::sleep` guarantees *at least* the requested
    /// duration, so the idle/probe margins are deterministic, not flaky.
    #[tokio::test]
    async fn sweep_aborts_writer_and_evicts_session_when_client_goes_silent() {
        let config = NostrServerTransportConfig::default()
            .with_open_stream(OpenStreamConfig::default().with_enabled(true));
        let pool: Arc<dyn RelayPoolTrait> = Arc::new(MockRelayPool::new());
        let mut transport = NostrServerTransport::with_relay_pool(config, pool)
            .await
            .expect("server transport");

        let writer = keepalive_test_writer("tok-silent", 40, 60);
        writer.start().await.expect("start the stream");
        install_slot(&transport.open_stream, "evt-silent", writer.clone(), false);
        assert!(writer.is_active(), "writer starts active");

        // Install a session for the slot's client and arm the eviction callback.
        let pubkey_hex = transport
            .open_stream
            .lock_slots()
            .get("evt-silent")
            .expect("slot")
            .snapshot
            .client_pubkey
            .to_hex();
        let evicted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let evicted_cb = evicted.clone();
        transport.sessions.set_eviction_callback(Arc::new(move |_| {
            evicted_cb.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        transport
            .sessions
            .get_or_create_session(&pubkey_hex, false, &transport.event_routes)
            .await;
        assert!(
            transport.sessions.get_session(&pubkey_hex).await.is_some(),
            "session present before the sweep"
        );

        let relay_pool = Arc::clone(&transport.base.relay_pool);
        let encryption_mode = transport.config.encryption_mode;
        let gift_wrap_mode = transport.config.gift_wrap_mode;

        // Past the idle window: the sweep probes (idle → SendPing). The writer is
        // NOT aborted yet — it is waiting for a `pong` the silent client never sends.
        tokio::time::sleep(Duration::from_millis(60)).await;
        NostrServerTransport::sweep_open_stream_sessions(
            &transport.open_stream,
            &relay_pool,
            encryption_mode,
            gift_wrap_mode,
            &transport.sessions,
        )
        .await;
        assert!(
            writer.is_active(),
            "writer must remain active while a probe is in flight"
        );
        assert!(
            transport.sessions.get_session(&pubkey_hex).await.is_some(),
            "session must survive a mere probe (client not yet declared dead)"
        );

        // Past the probe timeout: the sweep aborts (probe deadline → Abort) and
        // evicts the dead client's session.
        tokio::time::sleep(Duration::from_millis(80)).await;
        NostrServerTransport::sweep_open_stream_sessions(
            &transport.open_stream,
            &relay_pool,
            encryption_mode,
            gift_wrap_mode,
            &transport.sessions,
        )
        .await;
        assert!(
            !writer.is_active(),
            "writer must abort after a silent client misses the probe deadline"
        );
        assert!(
            transport.sessions.get_session(&pubkey_hex).await.is_none(),
            "dead client's session must be evicted on probe timeout"
        );
        assert!(
            evicted.load(std::sync::atomic::Ordering::SeqCst),
            "eviction callback must fire on probe-timeout session release"
        );
    }

    // ── CEP-8 payment-interaction negotiation ───────────────────

    fn pi_tag(value: &str) -> Tag {
        Tag::custom(
            TagKind::Custom(tags::PAYMENT_INTERACTION.into()),
            vec![value.to_string()],
        )
    }

    fn negotiate(
        session: &mut ClientSession,
        tags: &[Tag],
        is_request: bool,
        supports_explicit_gating: bool,
    ) -> NegotiationOutcome {
        NostrServerTransport::negotiate_payment_interaction(
            session,
            tags,
            Some("tools/call"),
            is_request,
            supports_explicit_gating,
        )
    }

    #[test]
    fn negotiate_transparent_request_under_optional_policy() {
        let mut session = ClientSession::new(false);
        let outcome = negotiate(&mut session, &[pi_tag("transparent")], true, true);

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(
            session.requested_payment_interaction,
            Some(PaymentInteractionMode::Transparent)
        );
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::Transparent)
        );
    }

    #[test]
    fn negotiate_accepts_gating_under_optional_policy() {
        let mut session = ClientSession::new(false);
        let outcome = negotiate(&mut session, &[pi_tag("explicit_gating")], true, true);

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(
            session.requested_payment_interaction,
            Some(PaymentInteractionMode::ExplicitGating)
        );
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::ExplicitGating)
        );
        assert!(!session.has_disclosed_payment_interaction);
    }

    #[test]
    fn negotiate_rejects_gating_request_when_unsupported() {
        let mut session = ClientSession::new(false);
        let outcome = negotiate(&mut session, &[pi_tag("explicit_gating")], true, false);

        assert_eq!(
            outcome,
            NegotiationOutcome::RejectUnsupported {
                requested: PaymentInteractionMode::ExplicitGating
            }
        );
        // The request is recorded, but the session is left on the lifecycle the server can serve.
        assert_eq!(
            session.requested_payment_interaction,
            Some(PaymentInteractionMode::ExplicitGating)
        );
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::Transparent)
        );
    }

    #[test]
    fn negotiate_does_not_reject_a_notification_carrying_gating() {
        let mut session = ClientSession::new(false);
        let outcome = negotiate(&mut session, &[pi_tag("explicit_gating")], false, false);

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::Transparent)
        );
    }

    #[test]
    fn negotiate_absent_tag_inherits_current_mode() {
        let mut session = ClientSession::new(false);
        session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.effective_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.has_disclosed_payment_interaction = true;

        let outcome = negotiate(&mut session, &[], true, true);

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(
            session.requested_payment_interaction,
            Some(PaymentInteractionMode::ExplicitGating)
        );
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::ExplicitGating)
        );
        assert!(session.has_disclosed_payment_interaction);
    }

    #[test]
    fn negotiate_initialize_resets_payment_fields_only() {
        let mut session = ClientSession::new(false);
        session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.effective_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.has_disclosed_payment_interaction = true;
        session.supports_encryption = true;
        session.supports_ephemeral_encryption = true;
        session.supports_oversized_transfer = true;
        session.supports_open_stream = true;
        session.is_initialized = true;
        session.has_sent_common_tags = true;

        let outcome = NostrServerTransport::negotiate_payment_interaction(
            &mut session,
            &[],
            Some("initialize"),
            true,
            true,
        );

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(session.requested_payment_interaction, None);
        assert_eq!(session.effective_payment_interaction, None);
        assert!(!session.has_disclosed_payment_interaction);
        // The learned transport capabilities must survive a renegotiation.
        assert!(session.supports_encryption);
        assert!(session.supports_ephemeral_encryption);
        assert!(session.supports_oversized_transfer);
        assert!(session.supports_open_stream);
        assert!(session.is_initialized);
        assert!(session.has_sent_common_tags);
    }

    #[test]
    fn negotiate_initialize_rederives_from_its_own_tag() {
        let mut session = ClientSession::new(false);
        session.effective_payment_interaction = Some(PaymentInteractionMode::Transparent);
        session.has_disclosed_payment_interaction = true;

        let outcome = NostrServerTransport::negotiate_payment_interaction(
            &mut session,
            &[pi_tag("explicit_gating")],
            Some("initialize"),
            true,
            true,
        );

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::ExplicitGating)
        );
        assert!(!session.has_disclosed_payment_interaction);
    }

    #[test]
    fn negotiate_mid_session_flip_rearms_disclosure() {
        let mut session = ClientSession::new(false);
        session.requested_payment_interaction = Some(PaymentInteractionMode::Transparent);
        session.effective_payment_interaction = Some(PaymentInteractionMode::Transparent);
        session.has_disclosed_payment_interaction = true;

        let outcome = negotiate(&mut session, &[pi_tag("explicit_gating")], true, true);

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::ExplicitGating)
        );
        assert!(!session.has_disclosed_payment_interaction);
    }

    #[test]
    fn negotiate_resent_same_mode_is_idempotent() {
        let mut session = ClientSession::new(false);
        session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.effective_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.has_disclosed_payment_interaction = true;

        let outcome = negotiate(&mut session, &[pi_tag("explicit_gating")], true, true);

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::ExplicitGating)
        );
        // Unchanged mode must not re-arm disclosure.
        assert!(session.has_disclosed_payment_interaction);
    }

    #[test]
    fn negotiate_unrecognized_value_downgrades() {
        let mut session = ClientSession::new(false);
        let outcome = negotiate(&mut session, &[pi_tag("bogus")], true, true);

        assert_eq!(outcome, NegotiationOutcome::Proceed);
        assert_eq!(
            session.requested_payment_interaction,
            Some(PaymentInteractionMode::Transparent)
        );
        assert_eq!(
            session.effective_payment_interaction,
            Some(PaymentInteractionMode::Transparent)
        );
    }

    #[test]
    fn unsupported_payment_interaction_error_pins_original_request_id() {
        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(42),
            method: "tools/call".to_string(),
            params: None,
        });

        let error = NostrServerTransport::unsupported_payment_interaction_error(
            &request,
            PaymentInteractionMode::ExplicitGating,
        );

        let JsonRpcMessage::ErrorResponse(response) = error else {
            panic!("expected an error response");
        };
        // The client correlates on the id it sent, not on the carrying Nostr event id.
        assert_eq!(response.id, serde_json::json!(42));
        assert_eq!(response.error.code, -32602);
        assert_eq!(
            response.error.message,
            "Unsupported payment_interaction mode: explicit_gating"
        );
        assert_eq!(
            response.error.data,
            Some(serde_json::json!({
                "requested": "explicit_gating",
                "supported": ["transparent"],
            }))
        );
    }

    #[test]
    fn unsupported_payment_interaction_error_preserves_string_id() {
        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("req-abc"),
            method: "tools/call".to_string(),
            params: None,
        });

        let error = NostrServerTransport::unsupported_payment_interaction_error(
            &request,
            PaymentInteractionMode::ExplicitGating,
        );

        let JsonRpcMessage::ErrorResponse(response) = error else {
            panic!("expected an error response");
        };
        assert_eq!(response.id, serde_json::json!("req-abc"));
    }

    // ── CEP-8 disclosure latch ──────────────────────────────────

    #[test]
    fn discloses_once_when_gating_requested() {
        let mut session = ClientSession::new(false);
        session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.effective_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);

        assert_eq!(
            NostrServerTransport::take_payment_interaction_disclosure(&mut session),
            Some(PaymentInteractionMode::ExplicitGating)
        );
        assert!(session.has_disclosed_payment_interaction);
        // Latched: a second response carries no disclosure.
        assert_eq!(
            NostrServerTransport::take_payment_interaction_disclosure(&mut session),
            None
        );
    }

    #[test]
    fn discloses_downgraded_effective_mode() {
        let mut session = ClientSession::new(false);
        session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.effective_payment_interaction = Some(PaymentInteractionMode::Transparent);

        assert_eq!(
            NostrServerTransport::take_payment_interaction_disclosure(&mut session),
            Some(PaymentInteractionMode::Transparent)
        );
    }

    #[test]
    fn no_disclosure_when_transparent_requested() {
        let mut session = ClientSession::new(false);
        session.requested_payment_interaction = Some(PaymentInteractionMode::Transparent);
        session.effective_payment_interaction = Some(PaymentInteractionMode::Transparent);

        assert_eq!(
            NostrServerTransport::take_payment_interaction_disclosure(&mut session),
            None
        );
        assert!(!session.has_disclosed_payment_interaction);
    }

    #[test]
    fn no_disclosure_before_effective_is_set() {
        let mut session = ClientSession::new(false);
        session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);

        assert_eq!(
            NostrServerTransport::take_payment_interaction_disclosure(&mut session),
            None
        );
        assert!(!session.has_disclosed_payment_interaction);
    }

    #[test]
    fn no_disclosure_for_a_client_that_never_asked() {
        let mut session = ClientSession::new(false);
        session.effective_payment_interaction = Some(PaymentInteractionMode::Transparent);

        assert_eq!(
            NostrServerTransport::take_payment_interaction_disclosure(&mut session),
            None
        );
    }

    // ── CEP-8 capability-list result shape ──────────────────────

    #[test]
    fn capability_list_results_are_recognized() {
        for key in ["tools", "resources", "resourceTemplates", "prompts"] {
            let result = serde_json::json!({ key: [] });
            assert!(
                NostrServerTransport::is_capability_list_result(&result),
                "{key} list result must be recognized"
            );
        }
    }

    #[test]
    fn non_list_results_are_not_recognized() {
        // A tools/call result.
        assert!(!NostrServerTransport::is_capability_list_result(
            &serde_json::json!({ "content": [{ "type": "text", "text": "hi" }], "isError": false })
        ));
        // An initialize result.
        assert!(!NostrServerTransport::is_capability_list_result(
            &serde_json::json!({ "protocolVersion": "2025-06-18", "capabilities": {} })
        ));
        // Empty and non-object results.
        assert!(!NostrServerTransport::is_capability_list_result(
            &serde_json::json!({})
        ));
        assert!(!NostrServerTransport::is_capability_list_result(
            &serde_json::json!("tools")
        ));
    }

    #[test]
    fn list_key_must_hold_an_array() {
        assert!(!NostrServerTransport::is_capability_list_result(
            &serde_json::json!({ "tools": {} })
        ));
        assert!(!NostrServerTransport::is_capability_list_result(
            &serde_json::json!({ "tools": null })
        ));
    }

    #[test]
    fn result_carrying_content_and_a_list_is_recognized() {
        // The ts-sdk list schemas ignore unknown keys, so a result carrying both `content` and a
        // list array parses as a list result there too. No `content` exclusion.
        assert!(NostrServerTransport::is_capability_list_result(
            &serde_json::json!({ "content": [], "resources": [] })
        ));
        assert!(NostrServerTransport::is_capability_list_result(
            &serde_json::json!({ "content": [{ "type": "text", "text": "hi" }], "tools": [] })
        ));
    }

    #[tokio::test]
    async fn deferred_open_stream_response_discloses_effective_mode() {
        // Guards the deferred flush path, which the normal-response tests do not reach.
        let pool = Arc::new(MockRelayPool::new());
        let transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_open_stream(OpenStreamConfig::default().with_enabled(true)),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");

        let client_keys = Keys::generate();
        let client_pubkey = client_keys.public_key();
        let client_pubkey_hex = client_pubkey.to_hex();

        // A session that negotiated the gated lifecycle and has not been told the outcome yet.
        {
            let mut sessions_w = transport.sessions.write().await;
            let mut session = ClientSession::new(false);
            session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
            session.effective_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
            sessions_w.put(client_pubkey_hex.clone(), session);
        }

        let event_id = EventId::all_zeros();
        let snapshot = RouteSnapshot {
            client_pubkey,
            original_request_id: serde_json::json!("streamed-1"),
            is_encrypted: false,
            mirrored_wrap_kind: None,
        };
        NostrServerTransport::publish_open_stream_deferred_response(
            &transport.base,
            GiftWrapMode::Optional,
            &event_id.to_hex(),
            &snapshot,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("streamed-1"),
                result: serde_json::json!({ "content": [] }),
            }),
            &transport.sessions,
            &ResponseTagSources {
                common: transport.announcement_manager.get_common_tags(),
                pricing: transport.announcement_manager.get_pricing_tags().to_vec(),
            },
        )
        .await
        .expect("publish the deferred response");

        let published = pool.stored_events().await;
        let response = published
            .iter()
            .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
            .expect("deferred response must be published");
        let disclosed: Vec<String> = response
            .tags
            .iter()
            .filter_map(|t| {
                let parts = t.clone().to_vec();
                match (parts.first().map(String::as_str), parts.get(1)) {
                    (Some(name), Some(v)) if name == tags::PAYMENT_INTERACTION => Some(v.clone()),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(
            disclosed,
            vec!["explicit_gating"],
            "a deferred response must disclose the effective mode exactly once"
        );

        // The latch is consumed, so a later response does not disclose again.
        let snap = transport
            .sessions
            .get_session(&client_pubkey_hex)
            .await
            .expect("session");
        assert!(snap.has_disclosed_payment_interaction);
    }

    #[tokio::test]
    async fn deferred_open_stream_response_carries_discovery_tags() {
        // The deferred flush is a first response like any other. If it carried only routing tags,
        // a stateless client would capture it as its session baseline and then report no server
        // name and no capabilities for the rest of the session, because the baseline only ever
        // upgrades to an event carrying a full initialize result.
        let pool = Arc::new(MockRelayPool::new());
        let transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_encryption_mode(EncryptionMode::Optional)
                .with_server_info(
                    ServerInfo::default()
                        .with_name("Deferred-Server")
                        .with_about("about-text")
                        .with_website("https://example.invalid")
                        .with_picture("https://example.invalid/p.png"),
                )
                .with_open_stream(OpenStreamConfig::default().with_enabled(true)),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");

        let client_pubkey = Keys::generate().public_key();
        {
            let mut sessions_w = transport.sessions.write().await;
            sessions_w.put(client_pubkey.to_hex(), ClientSession::new(false));
        }

        let snapshot = RouteSnapshot {
            client_pubkey,
            original_request_id: serde_json::json!(1),
            is_encrypted: false,
            mirrored_wrap_kind: None,
        };
        NostrServerTransport::publish_open_stream_deferred_response(
            &transport.base,
            GiftWrapMode::Optional,
            &EventId::all_zeros().to_hex(),
            &snapshot,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({ "content": [] }),
            }),
            &transport.sessions,
            &ResponseTagSources {
                common: transport.announcement_manager.get_common_tags(),
                pricing: transport.announcement_manager.get_pricing_tags().to_vec(),
            },
        )
        .await
        .expect("publish the deferred response");

        let published = pool.stored_events().await;
        let response = published
            .iter()
            .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
            .expect("deferred response must be published");
        // Pin the whole set, not a sample: the deferred path must send exactly what the normal
        // response path sends. A leaner helper that emits only the name and the capability flags
        // would satisfy any spot check while silently dropping the rest of the server identity,
        // and the client's baseline capture is sticky, so that loss would last the session.
        let sent: Vec<Vec<String>> = crate::transport::discovery_tags::get_discovery_tags(
            &response.tags.iter().cloned().collect::<Vec<Tag>>(),
        )
        .into_iter()
        .map(|t| t.to_vec())
        .collect();
        let expected: Vec<Vec<String>> = transport
            .announcement_manager
            .get_common_tags()
            .into_iter()
            .map(|t| t.to_vec())
            .collect();
        assert_eq!(
            sent, expected,
            "the deferred first response must carry the same tag set as the normal path"
        );
        for name in [tags::NAME, tags::ABOUT, tags::WEBSITE, tags::PICTURE] {
            assert!(
                sent.iter()
                    .any(|t| t.first().map(String::as_str) == Some(name)),
                "{name} must reach the client's session baseline"
            );
        }

        // One-shot: a second deferred response carries none of it.
        NostrServerTransport::publish_open_stream_deferred_response(
            &transport.base,
            GiftWrapMode::Optional,
            &EventId::all_zeros().to_hex(),
            &snapshot,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(2),
                result: serde_json::json!({ "content": [] }),
            }),
            &transport.sessions,
            &ResponseTagSources {
                common: transport.announcement_manager.get_common_tags(),
                pricing: transport.announcement_manager.get_pricing_tags().to_vec(),
            },
        )
        .await
        .expect("publish the second deferred response");
        let published = pool.stored_events().await;
        let second = published
            .iter()
            .rfind(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
            .expect("second response");
        assert_eq!(
            crate::core::serializers::get_tag_value(&second.tags, tags::NAME),
            None,
            "the discovery latch is one-shot"
        );
    }

    #[tokio::test]
    async fn deferred_open_stream_response_dedups_disclosure_against_advertisement() {
        // Now that the discovery set rides the deferred response, it can already carry the
        // server's availability advertisement naming this same mode. The disclosure must not add
        // a second copy.
        let pool = Arc::new(MockRelayPool::new());
        let mut transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_open_stream(OpenStreamConfig::default().with_enabled(true)),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");
        transport.set_announcement_extra_tags(vec![
            crate::payments::tags::payment_interaction_tag(PaymentInteractionMode::ExplicitGating),
        ]);

        let client_pubkey = Keys::generate().public_key();
        {
            let mut sessions_w = transport.sessions.write().await;
            let mut session = ClientSession::new(false);
            session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
            session.effective_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
            sessions_w.put(client_pubkey.to_hex(), session);
        }

        let snapshot = RouteSnapshot {
            client_pubkey,
            original_request_id: serde_json::json!(1),
            is_encrypted: false,
            mirrored_wrap_kind: None,
        };
        NostrServerTransport::publish_open_stream_deferred_response(
            &transport.base,
            GiftWrapMode::Optional,
            &EventId::all_zeros().to_hex(),
            &snapshot,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({ "content": [] }),
            }),
            &transport.sessions,
            &ResponseTagSources {
                common: transport.announcement_manager.get_common_tags(),
                pricing: transport.announcement_manager.get_pricing_tags().to_vec(),
            },
        )
        .await
        .expect("publish the deferred response");

        let published = pool.stored_events().await;
        let response = published
            .iter()
            .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
            .expect("deferred response must be published");
        let disclosed: Vec<String> = response
            .tags
            .iter()
            .filter_map(|t| {
                let parts = t.clone().to_vec();
                match (parts.first().map(String::as_str), parts.get(1)) {
                    (Some(name), Some(v)) if name == tags::PAYMENT_INTERACTION => Some(v.clone()),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(
            disclosed,
            vec!["explicit_gating"],
            "advertisement and disclosure must not both emit the same tag"
        );
    }

    #[tokio::test]
    async fn deferred_open_stream_response_prices_a_capability_list_result() {
        // A streaming tool may return a capability-list result. The normal response path prices
        // one, so this path must too, or the same content is tagged differently depending on
        // whether a stream happened to be open.
        let pool = Arc::new(MockRelayPool::new());
        let mut transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_open_stream(OpenStreamConfig::default().with_enabled(true)),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");
        transport.set_announcement_pricing_tags(
            crate::payments::tags::cap_tags_from_priced_capabilities(&[
                crate::payments::types::PricedCapability {
                    method: "tools/call".to_string(),
                    name: Some("get_weather".to_string()),
                    amount: 100,
                    max_amount: None,
                    currency_unit: "sats".to_string(),
                    description: None,
                },
            ]),
        );

        let client_pubkey = Keys::generate().public_key();
        {
            let mut sessions_w = transport.sessions.write().await;
            sessions_w.put(client_pubkey.to_hex(), ClientSession::new(false));
        }
        let snapshot = RouteSnapshot {
            client_pubkey,
            original_request_id: serde_json::json!(1),
            is_encrypted: false,
            mirrored_wrap_kind: None,
        };
        let sources = ResponseTagSources {
            common: transport.announcement_manager.get_common_tags(),
            pricing: transport.announcement_manager.get_pricing_tags().to_vec(),
        };

        let caps_on = |event: &Event| -> Vec<String> {
            event
                .tags
                .iter()
                .filter_map(|t| {
                    let parts = t.clone().to_vec();
                    match (parts.first().map(String::as_str), parts.get(1)) {
                        (Some(n), Some(v)) if n == tags::CAPABILITY => Some(v.clone()),
                        _ => None,
                    }
                })
                .collect()
        };

        // A list-shaped result is priced.
        NostrServerTransport::publish_open_stream_deferred_response(
            &transport.base,
            GiftWrapMode::Optional,
            &EventId::all_zeros().to_hex(),
            &snapshot,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({ "content": [], "tools": [] }),
            }),
            &transport.sessions,
            &sources,
        )
        .await
        .expect("publish the list response");
        let listed = pool.stored_events().await;
        let list_response = listed
            .iter()
            .rfind(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
            .expect("list response");
        assert_eq!(
            caps_on(list_response),
            vec!["tool:get_weather"],
            "a deferred capability-list result must carry the pricing tags"
        );

        // An ordinary tool result is not.
        NostrServerTransport::publish_open_stream_deferred_response(
            &transport.base,
            GiftWrapMode::Optional,
            &EventId::all_zeros().to_hex(),
            &snapshot,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(2),
                result: serde_json::json!({ "content": [], "isError": false }),
            }),
            &transport.sessions,
            &sources,
        )
        .await
        .expect("publish the plain response");
        let all = pool.stored_events().await;
        let plain = all
            .iter()
            .rfind(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
            .expect("plain response");
        assert!(
            caps_on(plain).is_empty(),
            "a non-list deferred result must not carry pricing tags"
        );
    }

    #[tokio::test]
    async fn deferred_open_stream_response_omits_tag_when_nothing_negotiated() {
        // A client that never asked for a non-default mode gets no tag, exactly as on the
        // normal response path.
        let pool = Arc::new(MockRelayPool::new());
        let transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_open_stream(OpenStreamConfig::default().with_enabled(true)),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");

        let client_pubkey = Keys::generate().public_key();
        {
            let mut sessions_w = transport.sessions.write().await;
            sessions_w.put(client_pubkey.to_hex(), ClientSession::new(false));
        }

        let snapshot = RouteSnapshot {
            client_pubkey,
            original_request_id: serde_json::json!(1),
            is_encrypted: false,
            mirrored_wrap_kind: None,
        };
        NostrServerTransport::publish_open_stream_deferred_response(
            &transport.base,
            GiftWrapMode::Optional,
            &EventId::all_zeros().to_hex(),
            &snapshot,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({ "content": [] }),
            }),
            &transport.sessions,
            &ResponseTagSources {
                common: transport.announcement_manager.get_common_tags(),
                pricing: transport.announcement_manager.get_pricing_tags().to_vec(),
            },
        )
        .await
        .expect("publish the deferred response");

        let published = pool.stored_events().await;
        let response = published
            .iter()
            .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
            .expect("deferred response must be published");
        assert!(
            !response.tags.iter().any(|t| {
                t.clone().to_vec().first().map(String::as_str) == Some(tags::PAYMENT_INTERACTION)
            }),
            "no disclosure when the client never negotiated"
        );
    }

    /// A raw `tools/call` request carrying a `_meta.progressToken`, which is what makes the event
    /// loop create a real writer (and with it the terminal hooks) for this event.
    fn streaming_call_event(
        client_keys: &Keys,
        server_pubkey: PublicKey,
        request_id: serde_json::Value,
        progress_token: &str,
        extra_tags: Vec<Tag>,
    ) -> Event {
        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: request_id,
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "streamer",
                "_meta": { "progressToken": progress_token },
            })),
        });
        let mut tags = BaseTransport::create_recipient_tags(&server_pubkey);
        tags.extend(extra_tags);
        crate::core::serializers::mcp_to_nostr_event(&request, CTXVM_MESSAGES_KIND, tags)
            .expect("serialize the streaming call")
            .sign_with_keys(client_keys)
            .expect("sign the streaming call")
    }

    /// A capability-list result, so the `cap` push is in play alongside discovery and disclosure.
    fn list_result_response(id: serde_json::Value) -> JsonRpcMessage {
        JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: serde_json::json!({ "tools": [] }),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deferred_response_tags_survive_the_writer_hook_wiring() {
        // The deferred-path tests above all call `publish_open_stream_deferred_response` directly,
        // handing it a `ResponseTagSources` the test built itself. That covers the composition but
        // not the wiring that supplies it in production: `start()` -> `event_loop` ->
        // `create_open_stream_writer` -> the two terminal hooks -> `flush_open_stream_response`.
        // Giving those hooks an empty tag set, or transposing `common` and `pricing` where they are
        // captured at `start()`, leaves every one of those tests green while shipping a deferred
        // first response with no server identity (the round-2 bug) or with prices in place of it.
        //
        // So this one takes the long way round: a real inbound `tools/call` with a progressToken, a
        // real writer taken from the slot the event loop created, and both flush orderings. The
        // assertion is the whole tag list in order, because the failure being guarded against is a
        // wrong tag SET, not a missing individual tag.
        let (client_pool, server_pool) = MockRelayPool::create_pair();
        let server_pubkey = server_pool.mock_public_key();
        let s_pool = Arc::new(server_pool);

        let mut transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_encryption_mode(EncryptionMode::Disabled)
                .with_server_info(
                    ServerInfo::default()
                        .with_name("Wired-Server")
                        .with_about("about-text")
                        .with_website("https://example.invalid")
                        .with_picture("https://example.invalid/p.png"),
                )
                .with_open_stream(OpenStreamConfig::default().with_enabled(true)),
            Arc::clone(&s_pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");
        // An optional policy accepts the client's `explicit_gating`, so the disclosure fires.
        transport.set_supported_payment_interaction(PaymentInteractionPolicy::Optional);
        // Pricing must be non-empty and disjoint from the common set, or a transposition of the
        // two would be indistinguishable from the correct wiring.
        transport.set_announcement_pricing_tags(
            crate::payments::tags::cap_tags_from_priced_capabilities(&[
                crate::payments::types::PricedCapability {
                    method: "tools/call".to_string(),
                    name: Some("streamer".to_string()),
                    amount: 100,
                    max_amount: None,
                    currency_unit: "sats".to_string(),
                    description: None,
                },
            ]),
        );

        let common = transport.announcement_manager.get_common_tags();
        let pricing = transport.announcement_manager.get_pricing_tags().to_vec();
        assert!(!common.is_empty() && !pricing.is_empty());

        let mut server_rx = transport
            .take_message_receiver()
            .expect("server message receiver");
        transport.start().await.expect("server start");
        tokio::time::sleep(Duration::from_millis(20)).await;

        // The full first-response tag list a deferred response must carry, in composition order:
        // routing -> CEP-35 discovery -> CEP-8 disclosure -> CEP-8 pricing.
        let expected_tags = |client_pubkey: &PublicKey, event_id: &str| -> Vec<Vec<String>> {
            let mut expected = vec![
                vec!["p".to_string(), client_pubkey.to_hex()],
                vec!["e".to_string(), event_id.to_string()],
            ];
            expected.extend(common.iter().map(|t| t.clone().to_vec()));
            expected.push(vec![
                tags::PAYMENT_INTERACTION.to_string(),
                "explicit_gating".to_string(),
            ]);
            expected.extend(pricing.iter().map(|t| t.clone().to_vec()));
            expected
        };

        // Both orderings of the same delivery: the response can be stashed before the stream ends
        // (flushed by the close hook, which reads the tag set captured at `start()`), or arrive
        // after it (delivered by `send_response`'s SendNow branch, which reads it live). They are
        // chosen by a race in production, so both have to be pinned.
        // The close and abort hooks are two separately-built closures, so each captures the tag
        // set on its own. Covering only one leaves the other free to ship an empty set.
        for (label, request_id, token, stash_before_close, abort_stream) in [
            (
                "ordering A (stashed, close hook flushes)",
                "stream-a",
                "tok-a",
                true,
                false,
            ),
            (
                "ordering B (stream terminal first)",
                "stream-b",
                "tok-b",
                false,
                false,
            ),
            (
                "ordering A (stashed, abort hook flushes)",
                "stream-c",
                "tok-c",
                true,
                true,
            ),
        ] {
            // A fresh client per ordering, so each gets an armed discovery latch.
            let client_keys = Keys::generate();
            let client_pubkey = client_keys.public_key();
            client_pool
                .publish_event(&streaming_call_event(
                    &client_keys,
                    server_pubkey,
                    serde_json::json!(request_id),
                    token,
                    vec![pi_tag("explicit_gating")],
                ))
                .await
                .expect("publish the streaming call");

            let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
                .await
                .expect("the request must reach the handler")
                .expect("channel closed");
            let event_id = incoming.event_id.clone();

            let writer = transport.get_open_stream_writer(&event_id).expect(
                "the event loop must create a writer for a tools/call with a progressToken",
            );
            writer.start().await.expect("start the stream");
            writer
                .write("chunk".to_string())
                .await
                .expect("stream a chunk");

            if stash_before_close {
                transport
                    .send_response(
                        &event_id,
                        list_result_response(serde_json::json!(request_id)),
                    )
                    .await
                    .expect("stash the deferred response");
                if abort_stream {
                    writer
                        .abort(Some("client cancelled".to_string()))
                        .await
                        .expect("abort the stream");
                } else {
                    writer.close().await.expect("close the stream");
                }
            } else {
                writer.close().await.expect("close the stream");
                transport
                    .send_response(
                        &event_id,
                        list_result_response(serde_json::json!(request_id)),
                    )
                    .await
                    .expect("deliver from the terminal slot");
            }

            // The response is the server-authored message carrying this request id; the stream
            // frames published on the same kind carry the progress token instead.
            let published = s_pool.stored_events().await;
            let response = published
                .iter()
                .find(|e| {
                    e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)
                        && e.pubkey == server_pubkey
                        && e.content.contains(request_id)
                })
                .unwrap_or_else(|| panic!("{label}: the deferred response must be published"));

            let sent: Vec<Vec<String>> = response.tags.iter().map(|t| t.clone().to_vec()).collect();
            assert_eq!(
                sent,
                expected_tags(&client_pubkey, &event_id),
                "{label}: a deferred first response must carry the same tag set, in the same order, \
                 as the normal response path"
            );
        }

        transport.close().await.expect("close the server");
    }

    // ── Targeted response sender ─────────────────────────────────────────────

    /// A server transport wired to `pool`, with the CEP-8 knobs the targeted-send tests need.
    async fn targeted_fixture(
        pool: &Arc<MockRelayPool>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        advertise_mode: bool,
        pricing: bool,
    ) -> NostrServerTransport {
        let mut transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_encryption_mode(encryption_mode)
                .with_gift_wrap_mode(gift_wrap_mode)
                .with_server_info(ServerInfo::default().with_name("Targeted-Server")),
            Arc::clone(pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");
        if advertise_mode {
            transport.set_announcement_extra_tags(vec![
                crate::payments::tags::payment_interaction_tag(
                    PaymentInteractionMode::ExplicitGating,
                ),
            ]);
        }
        if pricing {
            transport.set_announcement_pricing_tags(
                crate::payments::tags::cap_tags_from_priced_capabilities(&[
                    crate::payments::types::PricedCapability {
                        method: "tools/call".to_string(),
                        name: Some("get_weather".to_string()),
                        amount: 100,
                        max_amount: None,
                        currency_unit: "sats".to_string(),
                        description: None,
                    },
                ]),
            );
        }
        transport
    }

    /// Insert a session that negotiated `explicit_gating` and still owes the disclosure.
    async fn seed_gated_session(
        transport: &NostrServerTransport,
        client_pubkey_hex: &str,
        is_encrypted: bool,
    ) {
        let mut session = ClientSession::new(is_encrypted);
        session.requested_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        session.effective_payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        let mut sessions_w = transport.sessions.write().await;
        sessions_w.put(client_pubkey_hex.to_string(), session);
    }

    /// The shape of a payment-required error a gate answers with.
    fn gating_error(id: serde_json::Value) -> JsonRpcMessage {
        JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id,
            error: JsonRpcError {
                code: -32042,
                message: "Payment required".to_string(),
                data: None,
            },
        })
    }

    fn server_messages(events: &[Event], server_pubkey: PublicKey) -> Vec<&Event> {
        events
            .iter()
            .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND) && e.pubkey == server_pubkey)
            .collect()
    }

    fn tag_vecs(event: &Event) -> Vec<Vec<String>> {
        event.tags.iter().map(|t| t.clone().to_vec()).collect()
    }

    #[tokio::test]
    async fn targeted_response_correlates_to_the_request_event() {
        // The two routing arguments are both 64-character hex and either parses as the other's
        // type, so a transposed call site looks valid and then silently no-ops on the session
        // miss. Asserting both `p` and `e` is what catches that, because nothing is published.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey = Keys::generate().public_key();
        let client_pubkey_hex = client_pubkey.to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;

        let event_id = EventId::from_slice(&[7u8; 32]).expect("event id");
        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &event_id.to_hex(),
                gating_error(serde_json::json!("client-original-1")),
            )
            .await
            .expect("targeted send");

        let published = pool.stored_events().await;
        let messages = server_messages(&published, server_pubkey);
        assert_eq!(messages.len(), 1, "exactly one targeted response");
        let tags = tag_vecs(messages[0]);
        assert_eq!(
            tags.first(),
            Some(&vec!["p".to_string(), client_pubkey_hex.clone()]),
            "the response must be addressed to the client"
        );
        assert_eq!(
            tags.get(1),
            Some(&vec!["e".to_string(), event_id.to_hex()]),
            "the response must correlate to the request event"
        );

        // The id is the caller's, not the Nostr event id: this sender runs ahead of the worker
        // that rewrites ids, so the caller already holds the client's original.
        let payload: serde_json::Value =
            serde_json::from_str(&messages[0].content).expect("response is JSON");
        assert_eq!(payload["id"], serde_json::json!("client-original-1"));
        assert_eq!(payload["error"]["code"], serde_json::json!(-32042));
    }

    #[tokio::test]
    async fn targeted_response_leaves_the_route_intact() {
        // Answering a request must not end it. If the route were consumed, the eventual real
        // response would fail correlation and the client would wait forever.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;

        let client_pubkey = Keys::generate().public_key();
        let client_pubkey_hex = client_pubkey.to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;

        let event_id = EventId::from_slice(&[9u8; 32]).expect("event id");
        let event_id_hex = event_id.to_hex();
        transport
            .event_routes
            .register(
                event_id_hex.clone(),
                client_pubkey_hex.clone(),
                serde_json::json!("req-1"),
                None,
            )
            .await;

        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &event_id_hex,
                gating_error(serde_json::json!("req-1")),
            )
            .await
            .expect("targeted send");

        assert!(
            transport.event_routes.has_event_route(&event_id_hex).await,
            "the targeted send must leave the correlation route in place"
        );
        // And the consequence of leaving it: the real response still goes out.
        transport
            .send_response(
                &event_id_hex,
                JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: serde_json::json!("req-1"),
                    result: serde_json::json!({ "content": [] }),
                }),
            )
            .await
            .expect("the request must still be answerable normally");
    }

    #[tokio::test]
    async fn targeted_response_without_a_session_is_a_noop() {
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey_hex = Keys::generate().public_key().to_hex();
        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &EventId::all_zeros().to_hex(),
                gating_error(serde_json::json!("orphan-1")),
            )
            .await
            .expect("an evicted session is an ordinary race, not a transport failure");

        let published = pool.stored_events().await;
        assert!(
            server_messages(&published, server_pubkey).is_empty(),
            "nothing may be published for a client with no session"
        );
    }

    #[tokio::test]
    async fn a_malformed_routing_argument_costs_neither_latch() {
        // Both routing values arrive from an external caller, so a malformed one is reachable
        // input rather than an internal invariant. Validating before the session is touched is
        // what keeps it from consuming the one-shot latches for a response that is never
        // published; without that ordering the session silently loses its discovery replay and
        // its disclosure, and the next response carries neither.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey_hex = Keys::generate().public_key().to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;
        let good_event_id = EventId::from_slice(&[13u8; 32]).expect("event id").to_hex();

        // The pubkey is parsed first, so each argument gets its own failing call.
        transport
            .send_targeted_response(
                "not-a-pubkey",
                &good_event_id,
                gating_error(serde_json::json!("bad-pubkey-1")),
            )
            .await
            .expect_err("a malformed recipient must be an error, not a silent no-op");
        transport
            .send_targeted_response(
                &client_pubkey_hex,
                "not-an-event-id",
                gating_error(serde_json::json!("bad-event-id-1")),
            )
            .await
            .expect_err("a malformed event id must be an error, not a silent no-op");

        assert!(
            server_messages(&pool.stored_events().await, server_pubkey).is_empty(),
            "a malformed argument must publish nothing"
        );

        // Assert the consequence instead of the latch fields: the next well-formed send is
        // still a first response, so it carries both the discovery replay and the disclosure.
        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &good_event_id,
                gating_error(serde_json::json!("good-1")),
            )
            .await
            .expect("targeted send");

        let published = pool.stored_events().await;
        let messages = server_messages(&published, server_pubkey);
        assert_eq!(messages.len(), 1);
        let names: Vec<String> = tag_vecs(messages[0])
            .into_iter()
            .filter_map(|t| t.first().cloned())
            .collect();
        assert!(
            names.iter().any(|n| n == "name"),
            "the failed calls must not have burned the discovery-replay latch: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == tags::PAYMENT_INTERACTION),
            "the failed calls must not have burned the disclosure latch: {names:?}"
        );
    }

    #[tokio::test]
    async fn targeted_response_discloses_at_most_once() {
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey_hex = Keys::generate().public_key().to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;

        for (n, id) in ["once-1", "once-2"].iter().enumerate() {
            transport
                .send_targeted_response(
                    &client_pubkey_hex,
                    &EventId::from_slice(&[n as u8 + 1; 32])
                        .expect("event id")
                        .to_hex(),
                    gating_error(serde_json::json!(id)),
                )
                .await
                .expect("targeted send");
        }

        let published = pool.stored_events().await;
        let messages = server_messages(&published, server_pubkey);
        assert_eq!(messages.len(), 2);
        let disclosed = |e: &Event| {
            tag_vecs(e)
                .into_iter()
                .filter(|t| t.first().map(String::as_str) == Some(tags::PAYMENT_INTERACTION))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            disclosed(messages[0]),
            vec![vec![
                tags::PAYMENT_INTERACTION.to_string(),
                "explicit_gating".to_string()
            ]],
            "the first targeted response discloses the effective mode"
        );
        assert!(
            disclosed(messages[1]).is_empty(),
            "the disclosure is a one-shot per negotiated mode"
        );

        let snapshot = transport
            .sessions
            .get_session(&client_pubkey_hex)
            .await
            .expect("session");
        assert!(snapshot.has_disclosed_payment_interaction);
    }

    #[tokio::test]
    async fn targeted_response_mirrors_the_inbound_gift_wrap_kind() {
        // Without mirroring, an ephemeral request would be answered with a relay-stored wrap,
        // because the optional mode's fallback is the persistent kind.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;

        let client_keys = Keys::generate();
        let client_pubkey_hex = client_keys.public_key().to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, true).await;

        let event_id_hex = EventId::from_slice(&[3u8; 32]).expect("event id").to_hex();
        transport
            .request_wrap_kinds
            .write()
            .await
            .insert(event_id_hex.clone(), Some(EPHEMERAL_GIFT_WRAP_KIND));

        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &event_id_hex,
                gating_error(serde_json::json!("wrapped-1")),
            )
            .await
            .expect("targeted send");

        let published = pool.stored_events().await;
        let wrap = published
            .iter()
            .find(|e| {
                e.kind == Kind::Custom(GIFT_WRAP_KIND)
                    || e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)
            })
            .expect("the response must be gift-wrapped");
        assert_eq!(
            wrap.kind,
            Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND),
            "the targeted response must mirror the request's ephemeral wrap kind"
        );
    }

    #[tokio::test]
    async fn targeted_response_without_a_recorded_wrap_kind_falls_back() {
        // The map is populated before dispatch in production, so this drives the branch the
        // mirroring test cannot: no recorded kind, and the mode default applies.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;

        let client_pubkey_hex = Keys::generate().public_key().to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, true).await;

        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &EventId::from_slice(&[4u8; 32]).expect("event id").to_hex(),
                gating_error(serde_json::json!("unwrapped-1")),
            )
            .await
            .expect("targeted send");

        let published = pool.stored_events().await;
        let wrap = published
            .iter()
            .find(|e| {
                e.kind == Kind::Custom(GIFT_WRAP_KIND)
                    || e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)
            })
            .expect("the response must be gift-wrapped");
        assert_eq!(
            wrap.kind,
            Kind::Custom(GIFT_WRAP_KIND),
            "with no recorded request wrap kind the mode default applies"
        );
    }

    #[tokio::test]
    async fn targeted_error_response_carries_no_pricing_tags() {
        // The `cap` gate is shape-based and only fires on a result, so pricing is unobservable
        // for every error a gate sends. This asserts that.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            true,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey_hex = Keys::generate().public_key().to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;

        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &EventId::from_slice(&[5u8; 32]).expect("event id").to_hex(),
                gating_error(serde_json::json!("priced-1")),
            )
            .await
            .expect("targeted send");

        let published = pool.stored_events().await;
        let messages = server_messages(&published, server_pubkey);
        assert_eq!(messages.len(), 1);
        assert!(
            !tag_vecs(messages[0])
                .iter()
                .any(|t| t.first().map(String::as_str) == Some(tags::CAPABILITY)),
            "an error response never reaches the pricing gate"
        );
    }

    #[tokio::test]
    async fn targeted_response_after_a_first_response_replays_no_discovery() {
        // The discovery replay is one-shot per session and shared across all three response
        // paths, so whichever response goes out first carries it.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey = Keys::generate().public_key();
        let client_pubkey_hex = client_pubkey.to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;

        let first_event_id = EventId::from_slice(&[1u8; 32]).expect("event id").to_hex();
        transport
            .event_routes
            .register(
                first_event_id.clone(),
                client_pubkey_hex.clone(),
                serde_json::json!("normal-1"),
                None,
            )
            .await;
        transport
            .send_response(
                &first_event_id,
                JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: serde_json::json!("normal-1"),
                    result: serde_json::json!({ "content": [] }),
                }),
            )
            .await
            .expect("normal send");

        let second_event_id = EventId::from_slice(&[2u8; 32]).expect("event id").to_hex();
        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &second_event_id,
                gating_error(serde_json::json!("targeted-1")),
            )
            .await
            .expect("targeted send");

        let published = pool.stored_events().await;
        let targeted = server_messages(&published, server_pubkey)
            .into_iter()
            .find(|e| e.content.contains("targeted-1"))
            .expect("targeted response missing");
        assert_eq!(
            tag_vecs(targeted),
            vec![
                vec!["p".to_string(), client_pubkey_hex.clone()],
                vec!["e".to_string(), second_event_id],
            ],
            "the discovery replay and the disclosure both rode the earlier response"
        );
    }

    #[tokio::test]
    async fn targeted_response_dedups_disclosure_against_the_advertisement() {
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            true,
            false,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey = Keys::generate().public_key();
        let client_pubkey_hex = client_pubkey.to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;

        let event_id_hex = EventId::from_slice(&[6u8; 32]).expect("event id").to_hex();
        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &event_id_hex,
                gating_error(serde_json::json!("dedup-1")),
            )
            .await
            .expect("targeted send");

        let published = pool.stored_events().await;
        let messages = server_messages(&published, server_pubkey);
        assert_eq!(messages.len(), 1);
        // The whole list: the advertisement rides the discovery set and satisfies the
        // disclosure on its own, so no second copy is appended.
        assert_eq!(
            tag_vecs(messages[0]),
            vec![
                vec!["p".to_string(), client_pubkey_hex],
                vec!["e".to_string(), event_id_hex],
                vec!["name".to_string(), "Targeted-Server".to_string()],
                vec![
                    tags::PAYMENT_INTERACTION.to_string(),
                    "explicit_gating".to_string()
                ],
                vec![tags::SUPPORT_OVERSIZED_TRANSFER.to_string()],
            ],
            "advertisement and disclosure must not both emit the same tag"
        );
    }

    #[tokio::test]
    async fn injected_sender_matches_the_method() {
        // Two forms of one behavior is the shape this sender exists to avoid, so the claim that
        // they cannot diverge is executable: same inputs, byte-identical tags and the same kind.
        async fn publish_both(
            is_encrypted: bool,
            recorded_wrap_kind: Option<u16>,
            client_pubkey_hex: &str,
            event_id_hex: &str,
        ) -> (Vec<Vec<String>>, Kind, Vec<Vec<String>>, Kind) {
            let mut out = Vec::new();
            for via_closure in [false, true] {
                let pool = Arc::new(MockRelayPool::new());
                let transport = targeted_fixture(
                    &pool,
                    EncryptionMode::Optional,
                    GiftWrapMode::Optional,
                    false,
                    true,
                )
                .await;
                seed_gated_session(&transport, client_pubkey_hex, is_encrypted).await;
                if let Some(kind) = recorded_wrap_kind {
                    transport
                        .request_wrap_kinds
                        .write()
                        .await
                        .insert(event_id_hex.to_string(), Some(kind));
                }

                let response = gating_error(serde_json::json!("both-1"));
                if via_closure {
                    let send = transport.targeted_response_sender();
                    send(
                        client_pubkey_hex.to_string(),
                        event_id_hex.to_string(),
                        response,
                    )
                    .await
                    .expect("closure send");
                } else {
                    transport
                        .send_targeted_response(client_pubkey_hex, event_id_hex, response)
                        .await
                        .expect("method send");
                }

                let published = pool.stored_events().await;
                let event = published.first().expect("one published event").clone();
                out.push((tag_vecs(&event), event.kind));
            }
            let (closure_tags, closure_kind) = out.pop().expect("closure result");
            let (method_tags, method_kind) = out.pop().expect("method result");
            (method_tags, method_kind, closure_tags, closure_kind)
        }

        let client_pubkey_hex = Keys::generate().public_key().to_hex();
        let event_id_hex = EventId::from_slice(&[8u8; 32]).expect("event id").to_hex();

        // Plaintext: the published event carries the composed response tags directly.
        let (method_tags, method_kind, closure_tags, closure_kind) =
            publish_both(false, None, &client_pubkey_hex, &event_id_hex).await;
        assert_eq!(
            method_tags, closure_tags,
            "both forms must compose the same tag list"
        );
        assert_eq!(
            method_kind, closure_kind,
            "both forms must use the same kind"
        );
        assert!(
            method_tags
                .iter()
                .any(|t| t.first().map(String::as_str) == Some(tags::PAYMENT_INTERACTION)),
            "the fixture must arm the disclosure, or the comparison proves nothing about it"
        );
        assert!(
            method_tags
                .iter()
                .any(|t| t.first().map(String::as_str) == Some("name")),
            "the fixture must arm the discovery replay"
        );

        // Encrypted: the wrap kind is what differs if the closure skips its own lookup.
        let (_, method_kind, _, closure_kind) = publish_both(
            true,
            Some(EPHEMERAL_GIFT_WRAP_KIND),
            &client_pubkey_hex,
            &event_id_hex,
        )
        .await;
        assert_eq!(
            method_kind,
            Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND),
            "the method mirrors the request's wrap kind"
        );
        assert_eq!(
            closure_kind, method_kind,
            "the closure must mirror it too, through its own lookup"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_sender_works_from_a_detached_task() {
        // The shape a middleware uses: the sender outlives the borrow it came from and runs on a
        // task of its own, so `Send + 'static` has to hold behaviorally and not only at compile
        // time.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            false,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey = Keys::generate().public_key();
        let client_pubkey_hex = client_pubkey.to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;
        let event_id_hex = EventId::from_slice(&[10u8; 32]).expect("event id").to_hex();

        let send = transport.targeted_response_sender();
        let spawned_pubkey = client_pubkey_hex.clone();
        let spawned_event_id = event_id_hex.clone();
        tokio::spawn(async move {
            send(
                spawned_pubkey,
                spawned_event_id,
                gating_error(serde_json::json!("detached-1")),
            )
            .await
        })
        .await
        .expect("the detached task must not panic")
        .expect("the detached send must succeed");

        let published = pool.stored_events().await;
        let messages = server_messages(&published, server_pubkey);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            tag_vecs(messages[0]),
            vec![
                vec!["p".to_string(), client_pubkey_hex],
                vec!["e".to_string(), event_id_hex],
                vec!["name".to_string(), "Targeted-Server".to_string()],
                vec![tags::SUPPORT_OVERSIZED_TRANSFER.to_string()],
                vec![
                    tags::PAYMENT_INTERACTION.to_string(),
                    "explicit_gating".to_string()
                ],
            ],
            "a detached send composes the same tags as an inline one"
        );
    }

    #[tokio::test]
    async fn targeted_capability_list_result_carries_pricing_tags() {
        // The pricing gate is shape-based, so it is unreachable for the error responses a payment
        // gate sends. This sender is public, though, so a caller may legitimately answer with a
        // capability-list result, and that is the only way to pin that the targeted path threads
        // its pricing tags at all.
        let pool = Arc::new(MockRelayPool::new());
        let transport = targeted_fixture(
            &pool,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            false,
            true,
        )
        .await;
        let server_pubkey = pool.mock_public_key();

        let client_pubkey_hex = Keys::generate().public_key().to_hex();
        seed_gated_session(&transport, &client_pubkey_hex, false).await;

        transport
            .send_targeted_response(
                &client_pubkey_hex,
                &EventId::from_slice(&[11u8; 32]).expect("event id").to_hex(),
                JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: serde_json::json!("list-1"),
                    result: serde_json::json!({ "tools": [] }),
                }),
            )
            .await
            .expect("targeted send");

        let published = pool.stored_events().await;
        let messages = server_messages(&published, server_pubkey);
        assert_eq!(messages.len(), 1);
        let caps: Vec<Vec<String>> = tag_vecs(messages[0])
            .into_iter()
            .filter(|t| t.first().map(String::as_str) == Some(tags::CAPABILITY))
            .collect();
        assert_eq!(
            caps,
            vec![vec![
                tags::CAPABILITY.to_string(),
                "tool:get_weather".to_string(),
                "100".to_string(),
                "sats".to_string(),
            ]],
            "a capability-list result sent through the targeted path must carry the pricing tags"
        );
    }

    #[tokio::test]
    async fn injected_sender_captures_announcement_tags_when_it_is_built() {
        // The two forms share one publish, but not one view of the announcement tag sets: the
        // method reads them live, the closure captures them at construction. A caller that sets
        // the tags after building the sender therefore gets two different tag lists from what
        // looks like one behavior, which is the whole reason the constructor documents an
        // ordering rule the compiler cannot enforce.
        let pool = Arc::new(MockRelayPool::new());
        let mut transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_encryption_mode(EncryptionMode::Disabled)
                .with_gift_wrap_mode(GiftWrapMode::Optional),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");
        let server_pubkey = pool.mock_public_key();

        // Built BEFORE the announcement tags exist: this is the unsupported order.
        let send = transport.targeted_response_sender();
        transport.set_announcement_extra_tags(vec![Tag::custom(
            TagKind::Custom("late_tag".into()),
            vec!["late".to_string()],
        )]);

        let closure_client = Keys::generate().public_key().to_hex();
        let method_client = Keys::generate().public_key().to_hex();
        seed_gated_session(&transport, &closure_client, false).await;
        seed_gated_session(&transport, &method_client, false).await;
        let event_id = EventId::from_slice(&[12u8; 32]).expect("event id").to_hex();

        send(
            closure_client.clone(),
            event_id.clone(),
            gating_error(serde_json::json!("stale-1")),
        )
        .await
        .expect("closure send");
        transport
            .send_targeted_response(
                &method_client,
                &event_id,
                gating_error(serde_json::json!("fresh-1")),
            )
            .await
            .expect("method send");

        let published = pool.stored_events().await;
        let messages = server_messages(&published, server_pubkey);
        let carries_late_tag = |needle: &str| {
            let event = messages
                .iter()
                .find(|e| e.content.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing"));
            tag_vecs(event)
                .iter()
                .any(|t| t.first().map(String::as_str) == Some("late_tag"))
        };
        assert!(
            !carries_late_tag("stale-1"),
            "the closure replays the tag set captured when it was built"
        );
        assert!(
            carries_late_tag("fresh-1"),
            "the method reads the announcement tags live on every call"
        );
    }

    #[test]
    fn list_elements_are_not_validated_for_any_key() {
        // Documented looseness: ts validates the array elements against its schemas for all four
        // keys, so it would reject each of these malformed lists; the shape gate here accepts them.
        // `cap` is a reference signal, and a real MCP handler does not emit these.
        for key in ["tools", "resources", "resourceTemplates", "prompts"] {
            assert!(
                NostrServerTransport::is_capability_list_result(
                    &serde_json::json!({ key: [{ "foo": 1 }] })
                ),
                "{key} elements are not validated"
            );
        }
    }
}
