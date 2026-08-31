//! Client-side Nostr transport for ContextVM.
//!
//! Connects to a remote MCP server over Nostr. Sends JSON-RPC requests as
//! kind 25910 events, correlates responses via `e` tag.

pub mod correlation_store;
pub mod relay_resolution;
pub mod server_identity;
pub mod server_relay_discovery;

pub use correlation_store::ClientCorrelationStore;

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use nostr_sdk::prelude::*;
use tokio::sync::oneshot;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::core::constants::*;
use crate::core::error::Result;
use crate::core::serializers;
use crate::core::types::*;
use crate::core::validation;
use crate::encryption;
use crate::payments::client_payments::{is_payment_notification_method, ClientPaymentsEngine};
use crate::payments::constants::PAYMENT_REQUIRED_METHOD;
use crate::payments::tags::{parse_payment_interaction_value, payment_interaction_tag, pmi_tags};
use crate::relay::{RelayPool, RelayPoolTrait};
use crate::transport::base::BaseTransport;
use crate::transport::discovery_tags::{
    extract_payment_interaction, parse_discovered_peer_capabilities, PeerCapabilities,
};
use crate::transport::open_stream::{
    FrameOutcome, KeepaliveAction, OpenStreamConfig, OpenStreamFrame, OpenStreamReceiver,
    OpenStreamRegistry, OpenStreamSession, OpenStreamSessionInit, PublishFrame,
};
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
    /// When enabled, drives capability advertisement/learning, the inbound reader
    /// engine, the keepalive sweep, and `call_tool_stream`. Opt in with
    /// `OpenStreamConfig::enabled()` / `with_enabled(true)`.
    pub open_stream: OpenStreamConfig,
    /// CEP-8: the payment interaction mode this client requests for the session.
    ///
    /// Seeds the transport's negotiation state at construction;
    /// [`NostrClientTransport::set_payment_interaction`] wins on any later call.
    /// `None` (the default) means no `payment_interaction` tag is emitted and the
    /// session runs in the protocol default, `transparent`.
    pub payment_interaction: Option<PaymentInteractionMode>,
    /// CEP-8: payment method identifiers this client advertises, in preference order.
    ///
    /// Seeds the transport's negotiation state at construction;
    /// [`NostrClientTransport::set_client_pmis`] wins on any later call. Emitted as
    /// `pmi` tags on every outbound request (they are not one-shot).
    pub pmis: Vec<String>,
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
            payment_interaction: None,
            pmis: vec![],
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
    /// Set the full CEP-41 open-stream configuration (disabled by default; opt in
    /// with `OpenStreamConfig::enabled()`).
    pub fn with_open_stream(mut self, config: OpenStreamConfig) -> Self {
        self.open_stream = config;
        self
    }
    /// CEP-8: request a payment interaction mode for the session.
    ///
    /// Seeds the transport at construction. To change the mode after the transport
    /// exists, use [`NostrClientTransport::set_payment_interaction`], which wins over
    /// this value.
    pub fn with_payment_interaction(mut self, mode: PaymentInteractionMode) -> Self {
        self.payment_interaction = Some(mode);
        self
    }
    /// CEP-8: advertise payment method identifiers, in preference order.
    ///
    /// Seeds the transport at construction. To change the list after the transport
    /// exists, use [`NostrClientTransport::set_client_pmis`], which replaces this
    /// list rather than merging with it. An empty list emits no `pmi` tags.
    pub fn with_pmis(mut self, pmis: Vec<String>) -> Self {
        self.pmis = pmis;
        self
    }
}

/// CEP-8 client-side payment-interaction negotiation state.
///
/// All four fields live behind one mutex so the emission decision (does `requested`
/// differ from `last_sent`?) is one critical section: split across two locks, a
/// concurrent [`NostrClientTransport::set_payment_interaction`] could latch a mode
/// that was never published.
#[derive(Debug, Default)]
struct ClientNegotiationState {
    /// Payment method identifiers advertised on every negotiation-bearing request,
    /// in preference order.
    pmis: Vec<String>,
    /// The payment interaction mode this client requests for the session.
    requested: Option<PaymentInteractionMode>,
    /// The mode most recently published. The tag is re-emitted only when `requested`
    /// differs, so a routine invocation carries no `payment_interaction` tag (CEP-8
    /// asks implementations to omit repeated tags on routine invocations).
    last_sent: Option<PaymentInteractionMode>,
    /// The effective mode the server disclosed. Recorded only when this client
    /// requested `explicit_gating`; otherwise an inbound `payment_interaction` tag is
    /// a server availability advertisement rather than this session's mode.
    effective: Option<PaymentInteractionMode>,
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
    /// CEP-35: one-shot flag for client discovery tag emission. Shared (`Arc`)
    /// so [`ClientSendParts`] clones observe and flip the same latch; a cloned
    /// bare flag would fork it and re-send discovery tags on engine retries.
    has_sent_discovery_tags: Arc<AtomicBool>,
    /// CEP-35: learned server capabilities from inbound discovery tags.
    discovered_server_capabilities: Arc<Mutex<PeerCapabilities>>,
    /// CEP-35: first inbound event carrying discovery tags (session baseline).
    server_initialize_event: Arc<Mutex<Option<Event>>>,
    /// CEP-8: requested/advertised/observed payment-interaction negotiation state.
    /// Shared with the event loop, which records the server's disclosed mode into it.
    negotiation: Arc<Mutex<ClientNegotiationState>>,
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
    /// CEP-41: inbound reader engine for server→client streams (single peer).
    /// Outbound `call_tool_stream` sessions are created here too (each with a
    /// publish closure for consumer abort). Disposed on [`close`](Self::close).
    open_stream_registry: Arc<AsyncMutex<OpenStreamRegistry>>,
    /// CEP-41: FIFO of `call_tool_stream` placeholders awaiting their SDK-stamped
    /// progress token. [`send`](Self::send) binds the next one when a `tools/call`
    /// carrying a token is published (mirrors TS `pendingOutboundOpenStreamResolvers`).
    #[allow(clippy::type_complexity)]
    pending_outbound_open_stream:
        Arc<Mutex<VecDeque<oneshot::Sender<Result<(String, OpenStreamSession)>>>>>,
    /// CEP-41: monotonic `progress` for client→server control frames (`ping`/`pong`
    /// from the keepalive sweep / inbound handler). The server does not validate
    /// these, so one shared counter suffices.
    open_stream_control_progress: Arc<AtomicU64>,
    /// CEP-41: serializes the placeholder push→bind window of `call_tool_stream`.
    /// The placeholder FIFO is matched to a request by *order*, but rmcp stamps the
    /// token (and the worker publishes) in its own order — so two concurrent
    /// `call_tool_stream` calls could otherwise bind each other's tokens. Holding
    /// this across one call's push→bind keeps at most one placeholder unbound.
    open_stream_bind_lock: Arc<AsyncMutex<()>>,
    /// Channel for receiving processed MCP messages from the event loop.
    message_tx: Option<tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>>,
    message_rx: Option<tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>>,
    /// Token used to cancel the spawned event loop on close().
    cancellation_token: CancellationToken,
    /// Handle for the spawned event loop task.
    event_loop_handle: Option<tokio::task::JoinHandle<()>>,
    /// CEP-8: the client payments engine, installed by
    /// [`with_client_payments`](crate::payments::client_payments) before
    /// [`start`](Self::start) and invoked from the fixed inbound/outbound hooks.
    client_payments: Option<Arc<ClientPaymentsEngine>>,
}

/// CEP-41: a cheap, shareable handle to a client transport's open-stream state,
/// passed to [`call_tool_stream`](crate::call_tool_stream).
///
/// Obtained via [`NostrClientTransport::open_stream_handle`] before the transport
/// is moved into an rmcp service. It shares the transport's registry + placeholder
/// `Arc`s, so the served transport's `send` binds the placeholders this handle
/// pushes.
///
/// Only consumed through `call_tool_stream` (the `rmcp` feature); without it the
/// handle is dead but harmless.
#[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
#[derive(Clone)]
pub struct ClientOpenStreamHandle {
    registry: Arc<AsyncMutex<OpenStreamRegistry>>,
    #[allow(clippy::type_complexity)]
    pending: Arc<Mutex<VecDeque<oneshot::Sender<Result<(String, OpenStreamSession)>>>>>,
    bind_lock: Arc<AsyncMutex<()>>,
    config: OpenStreamConfig,
}

#[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
impl ClientOpenStreamHandle {
    /// Cancel an active open-stream reader session by its progress token.
    ///
    /// Send-safe equivalent of `ToolStreamCall::abort`: publishes the `abort`
    /// frame to the server and frees the reader-registry slot. Use this when the
    /// caller can't hold `&ToolStreamCall` across an `.await` — e.g. a stream
    /// driven inside `tokio::spawn`, where `ToolStreamCall`'s `!Sync` (its
    /// `result: BoxFuture` field) makes `abort(&self)` unusable from a `Send`
    /// task. This handle is `Sync`, so cloning it into a task and calling
    /// `cancel(token, reason)` works from any thread.
    ///
    /// Mirrors the `abort` path: `OpenStreamSession::abort` publishes the frame
    /// and finalizes the local stream, then `OpenStreamRegistry::consumer_abort`
    /// removes the entry and runs the `on_abort` hook. Both are idempotent, so
    /// canceling an unknown or already-terminated token is a harmless no-op.
    /// Without it, dropping a `ToolStreamCall` without aborting leaves the reader
    /// session lingering in the registry until the keepalive sweep probe-times it
    /// out (`Probe timeout`).
    pub async fn cancel(&self, progress_token: &str, reason: Option<String>) {
        let registry = self.registry.clone();
        // Clone the session out and release the lock before the publish await, so
        // the frame send doesn't block other open-stream operations.
        let session = registry.lock().await.get_session(progress_token);
        if let Some(session) = session {
            session.abort(reason.clone()).await;
        }
        registry
            .lock()
            .await
            .consumer_abort(progress_token, reason)
            .await;
    }

    /// Register a placeholder for the next outbound `call_tool_stream` session
    /// (resolved by the served transport's `send`).
    pub(crate) fn prepare_outbound(
        &self,
    ) -> oneshot::Receiver<Result<(String, OpenStreamSession)>> {
        let (tx, rx) = oneshot::channel();
        let mut pending = match self.pending.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        pending.push_back(tx);
        rx
    }

    /// Drop the most-recently registered (still-unbound) placeholder.
    ///
    /// Called by `call_tool_stream` when its request fails before the transport's
    /// `send` ever binds it — otherwise the orphaned placeholder would be popped
    /// (and mis-bound) by the next outbound `tools/call`. The bind lock guarantees
    /// only this call's placeholder can be unbound, so the back of the FIFO is it.
    pub(crate) fn cancel_outbound(&self) {
        let mut pending = match self.pending.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        pending.pop_back();
    }

    /// Clone the reader-registry handle (for the consumer `abort` path).
    pub(crate) fn registry(&self) -> Arc<AsyncMutex<OpenStreamRegistry>> {
        self.registry.clone()
    }

    /// The placeholder push→bind serialization lock (see the transport field).
    pub(crate) fn bind_lock(&self) -> &Arc<AsyncMutex<()>> {
        &self.bind_lock
    }

    /// The open-stream config (for the request-timeout options).
    pub(crate) fn config(&self) -> &OpenStreamConfig {
        &self.config
    }
}

impl std::fmt::Debug for ClientOpenStreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOpenStreamHandle")
            .finish_non_exhaustive()
    }
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
        let open_stream_registry = Arc::new(AsyncMutex::new(OpenStreamRegistry::with_policy(
            (&config.open_stream).into(),
        )));
        // Seeded before the struct literal below moves `config` by field shorthand.
        let negotiation = Arc::new(Mutex::new(ClientNegotiationState {
            pmis: config.pmis.clone(),
            requested: config.payment_interaction,
            ..Default::default()
        }));

        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            oversized_receiver,
            accept_waiters,
            original_progress_tokens,
            open_stream_registry,
            pending_outbound_open_stream: Arc::new(Mutex::new(VecDeque::new())),
            open_stream_control_progress: Arc::new(AtomicU64::new(0)),
            open_stream_bind_lock: Arc::new(AsyncMutex::new(())),
            config,
            server_pubkey,
            hinted_relay_urls,
            discovery_relay_urls,
            fallback_operational_relay_urls,
            pending_requests: ClientCorrelationStore::new(),
            has_sent_discovery_tags: Arc::new(AtomicBool::new(false)),
            discovered_server_capabilities: Arc::new(Mutex::new(PeerCapabilities::default())),
            server_initialize_event: Arc::new(Mutex::new(None)),
            server_supports_ephemeral: Arc::new(AtomicBool::new(false)),
            seen_gift_wrap_ids,
            negotiation,
            message_tx: Some(tx),
            message_rx: Some(rx),
            cancellation_token: CancellationToken::new(),
            event_loop_handle: None,
            client_payments: None,
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
        let open_stream_registry = Arc::new(AsyncMutex::new(OpenStreamRegistry::with_policy(
            (&config.open_stream).into(),
        )));
        // Seeded before the struct literal below moves `config` by field shorthand.
        let negotiation = Arc::new(Mutex::new(ClientNegotiationState {
            pmis: config.pmis.clone(),
            requested: config.payment_interaction,
            ..Default::default()
        }));

        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            oversized_receiver,
            accept_waiters,
            original_progress_tokens,
            open_stream_registry,
            pending_outbound_open_stream: Arc::new(Mutex::new(VecDeque::new())),
            open_stream_control_progress: Arc::new(AtomicU64::new(0)),
            open_stream_bind_lock: Arc::new(AsyncMutex::new(())),
            config,
            server_pubkey,
            hinted_relay_urls,
            discovery_relay_urls,
            fallback_operational_relay_urls,
            pending_requests: ClientCorrelationStore::new(),
            has_sent_discovery_tags: Arc::new(AtomicBool::new(false)),
            discovered_server_capabilities: Arc::new(Mutex::new(PeerCapabilities::default())),
            server_initialize_event: Arc::new(Mutex::new(None)),
            server_supports_ephemeral: Arc::new(AtomicBool::new(false)),
            seen_gift_wrap_ids,
            negotiation,
            message_tx: Some(tx),
            message_rx: Some(rx),
            cancellation_token: CancellationToken::new(),
            event_loop_handle: None,
            client_payments: None,
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
        let negotiation = self.negotiation.clone();
        let server_supports_ephemeral = self.server_supports_ephemeral.clone();
        let seen_gift_wrap_ids = self.seen_gift_wrap_ids.clone();
        let oversized_receiver = self.oversized_receiver.clone();
        let accept_waiters = self.accept_waiters.clone();
        let original_progress_tokens = self.original_progress_tokens.clone();
        let oversized_enabled = self.config.oversized_transfer.enabled;
        let open_stream_registry = self.open_stream_registry.clone();
        let open_stream_control_progress = self.open_stream_control_progress.clone();
        let open_stream_enabled = self.config.open_stream.enabled;
        let timeout = self.config.timeout;
        let client_payments = self.client_payments.clone();
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
                negotiation,
                server_supports_ephemeral,
                seen_gift_wrap_ids,
                oversized_receiver,
                accept_waiters,
                original_progress_tokens,
                oversized_enabled,
                open_stream_registry,
                open_stream_control_progress,
                open_stream_enabled,
                timeout,
                client_payments,
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
        // CEP-8: dispose payment state first so no detached payment task
        // outlives the transport (heartbeats, touch loops, caches, dedup).
        if let Some(ref engine) = self.client_payments {
            engine.dispose();
        }
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
        // CEP-41: dispose reader sessions and drop any unbound `call_tool_stream`
        // placeholders so their awaiters unblock (cancelled) instead of hanging.
        self.open_stream_registry.lock().await.clear();
        {
            let mut pending = match self.pending_outbound_open_stream.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            pending.clear();
        }
        self.base.disconnect().await
    }

    /// Send a JSON-RPC message to the server.
    pub async fn send(&self, message: &JsonRpcMessage) -> Result<()> {
        // CEP-8 outbound hook: record the raw request for explicit-gating
        // retries BEFORE any tag composition touches the send path, so a retry
        // replays exactly what the application sent. Engine-driven retries call
        // the send parts directly and deliberately skip this hook (their cache
        // entry already exists).
        if let Some(ref engine) = self.client_payments {
            engine.cache_raw_request(message);
        }
        self.send_parts().send(message).await
    }

    /// The transport's outbound send capability as detached cloned handles, so
    /// in-crate detached tasks (the payments engine's retries) can send through
    /// the full production path without holding `&self`.
    pub(crate) fn send_parts(&self) -> ClientSendParts {
        ClientSendParts {
            config: self.config.clone(),
            server_pubkey: self.server_pubkey,
            relay_pool: Arc::clone(&self.base.relay_pool),
            pending_requests: self.pending_requests.clone(),
            has_sent_discovery_tags: Arc::clone(&self.has_sent_discovery_tags),
            negotiation: Arc::clone(&self.negotiation),
            server_supports_ephemeral: Arc::clone(&self.server_supports_ephemeral),
            discovered_server_capabilities: Arc::clone(&self.discovered_server_capabilities),
            original_progress_tokens: Arc::clone(&self.original_progress_tokens),
            accept_waiters: Arc::clone(&self.accept_waiters),
            pending_outbound_open_stream: Arc::clone(&self.pending_outbound_open_stream),
            open_stream_registry: Arc::clone(&self.open_stream_registry),
            message_tx: self.message_tx.clone(),
        }
    }

    /// Whether [`start`](Self::start) has run (the event loop exists).
    pub(crate) fn is_started(&self) -> bool {
        self.event_loop_handle.is_some()
    }

    /// Whether [`close`](Self::close) has run (the consumer channel is gone).
    pub(crate) fn is_closed(&self) -> bool {
        self.message_tx.is_none()
    }

    /// A clone of the consumer channel sender, for in-crate hooks that push
    /// synthesized messages to the local consumer.
    pub(crate) fn consumer_sender(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>> {
        self.message_tx.clone()
    }

    /// A clone of the pending-request correlation store (shared handles), for
    /// the payments engine's keep-alive touch loop.
    pub(crate) fn correlation_store(&self) -> ClientCorrelationStore {
        self.pending_requests.clone()
    }

    /// The configured correlation-retention TTL (the payments engine bounds
    /// its touch cadence by half of it).
    pub(crate) fn correlation_timeout(&self) -> Duration {
        self.config.timeout
    }

    /// Whether a client payments engine is installed.
    pub(crate) fn client_payments_installed(&self) -> bool {
        self.client_payments.is_some()
    }

    /// Install the client payments engine (once, pre-start; the entry point
    /// guards both misuses before calling this).
    pub(crate) fn install_client_payments(&mut self, engine: Arc<ClientPaymentsEngine>) {
        self.client_payments = Some(engine);
    }

    /// CEP-22: record the original `_meta.progressToken` value of an
    /// outbound request under its stringified form, replacing any stale entry
    /// for the same key. See [`Self::original_progress_tokens`]. Production
    /// code records through [`ClientSendParts`]; this test-only wrapper keeps
    /// the in-module tests on the same write path.
    #[cfg(test)]
    fn record_original_progress_token(&self, token: &str, original: &serde_json::Value) {
        record_original_progress_token(&self.original_progress_tokens, token, original);
    }

    /// CEP-22: drop (and return) the original `progressToken` value
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
        negotiation: Arc<Mutex<ClientNegotiationState>>,
        server_supports_ephemeral: Arc<AtomicBool>,
        seen_gift_wrap_ids: Arc<Mutex<LruCache<EventId, ()>>>,
        oversized_receiver: Arc<Mutex<OversizedTransferReceiver>>,
        accept_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
        original_progress_tokens: Arc<Mutex<LruCache<String, serde_json::Value>>>,
        oversized_enabled: bool,
        open_stream_registry: Arc<AsyncMutex<OpenStreamRegistry>>,
        open_stream_control_progress: Arc<AtomicU64>,
        open_stream_enabled: bool,
        timeout: Duration,
        client_payments: Option<Arc<ClientPaymentsEngine>>,
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
                        &negotiation,
                        &server_supports_ephemeral,
                        &seen_gift_wrap_ids,
                        &oversized_receiver,
                        &accept_waiters,
                        &original_progress_tokens,
                        &open_stream_registry,
                        &open_stream_control_progress,
                        open_stream_enabled,
                        &client_payments,
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
                    // CEP-41: drive the open-stream keepalive (idle → ping; probe /
                    // close-grace deadline → abort) for every active reader session.
                    if open_stream_enabled {
                        let gift_wrap_kind = outbound_gift_wrap_kind(
                            gift_wrap_mode,
                            server_supports_ephemeral.load(Ordering::Relaxed),
                        );
                        Self::sweep_client_open_stream_sessions(
                            &open_stream_registry,
                            &open_stream_control_progress,
                            &relay_pool,
                            server_pubkey,
                            encryption_mode,
                            gift_wrap_kind,
                            Instant::now(),
                        )
                        .await;
                    }
                }
            }
        }
    }

    // ── CEP-35 discovery tag helpers ──────────────────────────────

    /// Constructs client capability tags based on config (test-only wrapper
    /// over [`ClientSendParts`], the production tag composer).
    #[cfg(test)]
    fn get_client_capability_tags(&self) -> Vec<Tag> {
        self.send_parts().get_client_capability_tags()
    }

    /// One-shot: returns capability tags if not yet sent, empty otherwise
    /// (test-only wrapper over [`ClientSendParts`]).
    #[cfg(test)]
    fn get_pending_client_discovery_tags(&self) -> Vec<Tag> {
        self.send_parts().get_pending_client_discovery_tags()
    }

    /// CEP-8: the negotiation tags pending for the next outbound request (see
    /// [`ClientSendParts::get_pending_negotiation_tags`] for the three rules;
    /// test-only wrapper).
    #[cfg(test)]
    fn get_pending_negotiation_tags(&self) -> (Vec<Tag>, Option<PaymentInteractionMode>) {
        self.send_parts().get_pending_negotiation_tags()
    }

    /// Parses inbound event tags and updates learned server capabilities.
    fn learn_server_discovery(
        discovered_caps: &Mutex<PeerCapabilities>,
        init_event: &Mutex<Option<Event>>,
        negotiation: &Mutex<ClientNegotiationState>,
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
            // CEP-41: OR-learn the server's open-stream support (never downgrades).
            caps.supports_open_stream |= discovered.capabilities.supports_open_stream;
        }

        // CEP-8: record the effective payment interaction mode the server disclosed.
        //
        // Placement is load-bearing, so do not move this block. Above the
        // `discovery_tags.is_empty()` early return it would be redundant (a
        // `payment_interaction` tag is itself a discovery tag, so an event carrying one
        // can never take that return), and below the baseline logic it would tangle
        // with the initialize-upgrade branch.
        //
        // The value is authoritative only when *this* client requested
        // `explicit_gating`. On any other session an inbound tag is a server
        // availability advertisement, and recording it would leave a transparent client
        // believing it is on the explicit-gating lifecycle. Note the gate is on
        // `ExplicitGating` specifically, not merely on a mode having been requested: a
        // client that explicitly requested `transparent` also ignores the tag.
        {
            let mut state = match negotiation.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if state.requested == Some(PaymentInteractionMode::ExplicitGating) {
                // The first tag wins, and an unrecognized value parses to `None`, which
                // leaves any previously recorded mode intact. The *observed* value is
                // recorded, never the requested one, so a disclosed downgrade to
                // `transparent` is stored as `transparent`.
                if let Some(value) = extract_payment_interaction(&tag_vec) {
                    if let Some(mode) = parse_payment_interaction_value(&value) {
                        state.effective = Some(mode);
                    }
                }
            }
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

    // ── CEP-8 payment interaction ─────────────────────────────────

    /// CEP-8: request a payment interaction mode for this session.
    ///
    /// The tag rides the next outbound request and is then latched: it is re-emitted
    /// only when the requested mode differs from the one most recently **published**,
    /// so routine invocations stay clean. Setting a mode and setting it back with no
    /// request published in between therefore emits nothing, because nothing changed
    /// on the wire.
    ///
    /// Deliberately `&self` and usable after [`start`](Self::start), unlike the
    /// server's [`set_supported_payment_interaction`][srv], which must be called
    /// before the server starts. Mid-session upsert is the point of the client side:
    /// a later tag establishes or changes the session's mode.
    ///
    /// Overrides [`NostrClientTransportConfig::payment_interaction`]; there is no
    /// merge.
    ///
    /// **Caller contract.** CEP-8 asks clients to keep `payment_interaction`
    /// consistent across concurrently in-flight requests. Two requests composed on
    /// either side of a mode change carry different modes, and which one the session
    /// ends on depends on relay delivery order. Quiesce in-flight requests before
    /// changing the mode mid-session; the transport deliberately does not serialize
    /// sends to enforce this, since that would hold state across a publish.
    ///
    /// [srv]: crate::transport::server::NostrServerTransport::set_supported_payment_interaction
    pub fn set_payment_interaction(&self, mode: PaymentInteractionMode) {
        let mut state = match self.negotiation.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        state.requested = Some(mode);
    }

    /// CEP-8: advertise payment method identifiers, in preference order.
    ///
    /// Replaces any previously configured list, including one set through
    /// [`NostrClientTransportConfig::pmis`]; there is no merge. The tags are not
    /// latched, so they ride every subsequent request. An empty list emits no tags.
    pub fn set_client_pmis(&self, pmis: Vec<String>) {
        let mut state = match self.negotiation.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        state.pmis = pmis;
    }

    /// CEP-8: the effective payment interaction mode the server disclosed, if any.
    ///
    /// Only recorded when this client itself requested `explicit_gating`. An inbound
    /// `payment_interaction` tag on any other session is a server *availability
    /// advertisement*, not this session's negotiated mode, and recording it would
    /// leave a transparent client believing it is on the explicit-gating lifecycle.
    /// The value is therefore authoritative only for a session in which this client
    /// requested gating.
    ///
    /// It can read stale in two ways. After this client downgrades itself with
    /// [`set_payment_interaction`](Self::set_payment_interaction), the gate above blocks
    /// further updates, so the value freezes at whatever was last observed. And a server
    /// transition to `transparent` is signalled by the *absence* of the tag, which a
    /// present-tag-only reader cannot see at all. Both match the TypeScript SDK, so the
    /// two implementations agree on the same wire trace.
    pub fn get_effective_payment_interaction(&self) -> Option<PaymentInteractionMode> {
        let state = match self.negotiation.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        state.effective
    }

    // ── CEP-41 open-stream ────────────────────────────────────────

    /// CEP-41: register a placeholder for the next outbound `call_tool_stream`
    /// session. [`send`](Self::send) binds it to the paired `tools/call`'s
    /// SDK-stamped progress token and resolves the returned receiver with
    /// `(progress_token, OpenStreamSession)` (or an error on admission failure /
    /// transport close).
    pub fn prepare_outbound_open_stream_session(
        &self,
    ) -> oneshot::Receiver<Result<(String, OpenStreamSession)>> {
        let (tx, rx) = oneshot::channel();
        let mut pending = match self.pending_outbound_open_stream.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        pending.push_back(tx);
        rx
    }

    /// CEP-41: the reader session for `token`, if one exists.
    pub async fn get_open_stream_session(&self, token: &str) -> Option<OpenStreamSession> {
        self.open_stream_registry.lock().await.get_session(token)
    }

    /// CEP-41: a cheap, shareable handle to this transport's open-stream state for
    /// [`call_tool_stream`](crate::call_tool_stream).
    ///
    /// Obtain it **before** moving the transport into an rmcp service (which
    /// consumes it): the handle clones the registry + placeholder `Arc`s, so the
    /// served transport's `send` still binds the placeholders the handle pushes.
    pub fn open_stream_handle(&self) -> ClientOpenStreamHandle {
        ClientOpenStreamHandle {
            registry: self.open_stream_registry.clone(),
            pending: self.pending_outbound_open_stream.clone(),
            bind_lock: self.open_stream_bind_lock.clone(),
            config: self.config.open_stream.clone(),
        }
    }

    /// CEP-41: consumer cancel for an outbound stream — publish an `abort` frame to
    /// the server (so its writer aborts), finalize the local stream, and free the
    /// registry slot. Exposed to consumers via `ToolStreamCall::abort`.
    pub async fn abort_open_stream(&self, token: &str, reason: Option<String>) {
        let session = { self.open_stream_registry.lock().await.get_session(token) };
        if let Some(session) = session {
            // Publishes the `abort` frame to the server + finalizes locally.
            session.abort(reason.clone()).await;
        }
        // Free the concurrency slot + run any hook (idempotent re-finalize).
        self.open_stream_registry
            .lock()
            .await
            .consumer_abort(token, reason)
            .await;
    }

    /// CEP-41 inbound interception (beside the oversized branch). Feeds the frame
    /// to the reader engine, publishes a `pong` on `SendPong`, forwards a stripped
    /// progress with the original token restored (resets rmcp's idle timer),
    /// and keeps the request correlation alive via `pending.touch`.
    #[allow(clippy::too_many_arguments)]
    async fn handle_inbound_open_stream_frame(
        open_stream_registry: &Arc<AsyncMutex<OpenStreamRegistry>>,
        open_stream_control_progress: &Arc<AtomicU64>,
        original_progress_tokens: &Mutex<LruCache<String, serde_json::Value>>,
        pending: &ClientCorrelationStore,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        server_pubkey: PublicKey,
        encryption_mode: EncryptionMode,
        gift_wrap_kind: u16,
        tx: &tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
        notif: &JsonRpcNotification,
        e_tag: Option<&str>,
    ) {
        let token = notif
            .params
            .as_ref()
            .and_then(|p| p.get("progressToken"))
            .and_then(progress_token_string);

        // Keep the request correlation alive — chunks don't otherwise refresh it.
        if let Some(correlated) = e_tag {
            pending.touch(correlated).await;
        }

        // Feed the reader engine (delivers the chunk to the consumer's stream).
        let outcome = {
            open_stream_registry
                .lock()
                .await
                .process_frame(Instant::now(), notif)
                .await
        };

        // SendPong → answer the peer's keepalive ping.
        if let Ok(FrameOutcome::SendPong(nonce)) = &outcome {
            if let Some(token) = token.as_deref() {
                let progress = open_stream_control_progress.fetch_add(1, Ordering::SeqCst) + 1;
                if let Ok(frame) = (OpenStreamFrame::Pong {
                    nonce: nonce.clone(),
                })
                .into_progress_notification(token, progress, None)
                {
                    let base = BaseTransport {
                        relay_pool: Arc::clone(relay_pool),
                        encryption_mode,
                        is_connected: true,
                    };
                    let tags = BaseTransport::create_recipient_tags(&server_pubkey);
                    let _ = base
                        .send_mcp_message(
                            &JsonRpcMessage::Notification(frame),
                            &server_pubkey,
                            CTXVM_MESSAGES_KIND,
                            tags,
                            None,
                            Some(gift_wrap_kind),
                        )
                        .await;
                }
            }
        }

        // Reset rmcp's idle timer: forward a stripped progress carrying the ORIGINAL
        // (numeric) token — the chunk itself already reached the consumer's stream.
        // Done before the terminal cleanup so the token is still recorded.
        if let Some(token) = token.as_deref() {
            Self::forward_stripped_progress(notif, token, original_progress_tokens, tx);
        }

        // Drop the recorded original token once the stream is terminal.
        if matches!(
            &outcome,
            Ok(FrameOutcome::Closed) | Ok(FrameOutcome::Aborted(_)) | Err(_)
        ) {
            Self::remove_original_progress_token(original_progress_tokens, token.as_deref());
        }
    }

    /// CEP-41 client keepalive sweep: drive each reader session's pure `tick(now)`;
    /// publish a `ping` on `SendPing` (an `Abort` already finalized + removed the
    /// session — the consumer sees the terminal error on the stream). `now` is a
    /// parameter so tests can drive idle→ping→probe deterministically.
    async fn sweep_client_open_stream_sessions(
        open_stream_registry: &Arc<AsyncMutex<OpenStreamRegistry>>,
        open_stream_control_progress: &Arc<AtomicU64>,
        relay_pool: &Arc<dyn RelayPoolTrait>,
        server_pubkey: PublicKey,
        encryption_mode: EncryptionMode,
        gift_wrap_kind: u16,
        now: Instant,
    ) {
        let actions = { open_stream_registry.lock().await.tick_all(now) };
        for (token, action) in actions {
            if let KeepaliveAction::SendPing(nonce) = action {
                let progress = open_stream_control_progress.fetch_add(1, Ordering::SeqCst) + 1;
                if let Ok(frame) = (OpenStreamFrame::Ping { nonce })
                    .into_progress_notification(&token, progress, None)
                {
                    let base = BaseTransport {
                        relay_pool: Arc::clone(relay_pool),
                        encryption_mode,
                        is_connected: true,
                    };
                    let tags = BaseTransport::create_recipient_tags(&server_pubkey);
                    let _ = base
                        .send_mcp_message(
                            &JsonRpcMessage::Notification(frame),
                            &server_pubkey,
                            CTXVM_MESSAGES_KIND,
                            tags,
                            None,
                            Some(gift_wrap_kind),
                        )
                        .await;
                }
            }
        }
    }

    /// CEP-41: run one keepalive sweep at `now`. The event loop drives this on its
    /// own timer with `Instant::now()`; it is also exposed so callers (and
    /// deterministic tests) can drive idle→ping→probe→abort with an explicit
    /// instant — the session clock is `std::time::Instant`, unaffected by
    /// `tokio`'s `start_paused`.
    pub async fn run_open_stream_keepalive_sweep(&self, now: Instant) {
        Self::sweep_client_open_stream_sessions(
            &self.open_stream_registry,
            &self.open_stream_control_progress,
            &self.base.relay_pool,
            self.server_pubkey,
            self.config.encryption_mode,
            self.choose_outbound_gift_wrap_kind(),
            now,
        )
        .await;
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
        negotiation: &Arc<Mutex<ClientNegotiationState>>,
        server_supports_ephemeral: &Arc<AtomicBool>,
        seen_gift_wrap_ids: &Arc<Mutex<LruCache<EventId, ()>>>,
        oversized_receiver: &Arc<Mutex<OversizedTransferReceiver>>,
        accept_waiters: &Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
        original_progress_tokens: &Arc<Mutex<LruCache<String, serde_json::Value>>>,
        open_stream_registry: &Arc<AsyncMutex<OpenStreamRegistry>>,
        open_stream_control_progress: &Arc<AtomicU64>,
        open_stream_enabled: bool,
        client_payments: &Option<Arc<ClientPaymentsEngine>>,
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
        Self::learn_server_discovery(discovered_caps, init_event, negotiation, &source_event);

        // CEP-19: learn ephemeral support from server
        if Self::should_learn_ephemeral_support(
            actual_pubkey,
            server_pubkey,
            if is_gift_wrap { Some(outer_kind) } else { None },
            &verified_tags,
        ) {
            server_supports_ephemeral.store(true, Ordering::Relaxed);
        }

        // CEP-41: intercept open-stream frames before the correlation gate, beside
        // the oversized branch. Type-disjoint (`is_open_stream_frame` vs
        // `is_oversized_frame` claim distinct `cvm.type`s). The engine delivers
        // chunks to the consumer's `OpenStreamSession`; here we also forward a
        // stripped progress to reset rmcp's idle timer and keep correlation alive.
        if open_stream_enabled {
            if let Ok(notif) = serde_json::from_str::<JsonRpcNotification>(&actual_event_content) {
                if notif.method == NOTIFICATIONS_PROGRESS_METHOD
                    && OpenStreamReceiver::is_open_stream_frame(&notif)
                {
                    let gift_wrap_kind = outbound_gift_wrap_kind(
                        gift_wrap_mode,
                        server_supports_ephemeral.load(Ordering::Relaxed),
                    );
                    Self::handle_inbound_open_stream_frame(
                        open_stream_registry,
                        open_stream_control_progress,
                        original_progress_tokens,
                        pending,
                        relay_pool,
                        server_pubkey,
                        encryption_mode,
                        gift_wrap_kind,
                        tx,
                        &notif,
                        e_tag.as_deref(),
                    )
                    .await;
                    return;
                }
            }
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
                        if e_tag.is_none() {
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
                        // The reassembled response is terminal: it flows through the
                        // SAME consume-terminal-response helper as the parse site, so
                        // a gating error delivered via CEP-22 frames is intercepted
                        // and a paid oversized result clears its payment state.
                        if let Some(deliverable) = Self::consume_terminal_response(
                            pending,
                            client_payments,
                            e_tag.as_deref(),
                            message,
                        )
                        .await
                        {
                            let _ = tx.send(deliverable);
                        }
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

            // CEP-8: a correlated `payment_required` refreshes its request's pending
            // entry so the retention sweep does not evict a request whose payment is
            // still settling; the real response can arrive minutes later.
            if let JsonRpcMessage::Notification(ref n) = mcp_msg {
                if n.method == PAYMENT_REQUIRED_METHOD {
                    if let Some(ref correlated_id) = e_tag {
                        pending.touch(correlated_id.as_str()).await;
                    }
                }
            }

            // Only a Response or ErrorResponse answers the pending request, so only
            // those consume the correlation entry. A correlated notification of any
            // method passes the read-only gate above and is forwarded WITHOUT
            // touching the entry: it must stay alive for the response that follows.
            // (The TS client classifies by JSON-RPC type the same way; its only
            // production delete is on the response path.) Terminal messages flow
            // through the shared consume-terminal-response helper, the same one the
            // oversized reassembly delivery uses.
            if matches!(
                mcp_msg,
                JsonRpcMessage::Response(_) | JsonRpcMessage::ErrorResponse(_)
            ) {
                if let Some(deliverable) = Self::consume_terminal_response(
                    pending,
                    client_payments,
                    e_tag.as_deref(),
                    mcp_msg,
                )
                .await
                {
                    let _ = tx.send(deliverable);
                }
            } else {
                // CEP-8 hook: a correlated payment notification goes through
                // the engine, which reacts (auto-pay pipeline, keep-alives,
                // synthesis) and decides forward-or-replace. Synthesized
                // messages are pushed by the engine BEFORE it returns, so the
                // consumer sees them first (the reference ordering).
                let forward = match (client_payments, &mcp_msg, &e_tag) {
                    (Some(engine), JsonRpcMessage::Notification(n), Some(correlated_id))
                        if is_payment_notification_method(&n.method) =>
                    {
                        let entry = pending.peek(correlated_id.as_str()).await;
                        let (requested_gating, effective_gating) = {
                            let state = match negotiation.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            (
                                state.requested == Some(PaymentInteractionMode::ExplicitGating),
                                state.effective == Some(PaymentInteractionMode::ExplicitGating),
                            )
                        };
                        engine.handle_payment_notification(
                            n,
                            correlated_id.as_str(),
                            entry,
                            requested_gating,
                            effective_gating,
                        )
                    }
                    _ => true,
                };
                if forward {
                    let _ = tx.send(mcp_msg);
                }
            }
        }
    }

    /// Consume the pending correlation entry for a terminal `Response` /
    /// `ErrorResponse` and run the payments engine's terminal hook. This is the
    /// ONE terminal consumption path, invoked at BOTH push sites (the parse site
    /// and the CEP-22 oversized reassembly delivery), so a gating error riding
    /// either path is classified exactly once and a paid request's payment state
    /// is cleared no matter which path delivered its result. Returns the message
    /// to deliver to the consumer, or `None` when the engine intercepted it.
    async fn consume_terminal_response(
        pending: &ClientCorrelationStore,
        client_payments: &Option<Arc<ClientPaymentsEngine>>,
        e_tag: Option<&str>,
        message: JsonRpcMessage,
    ) -> Option<JsonRpcMessage> {
        let entry = match (&message, e_tag) {
            (
                JsonRpcMessage::Response(_) | JsonRpcMessage::ErrorResponse(_),
                Some(correlated_id),
            ) => pending.remove(correlated_id).await,
            _ => None,
        };
        match client_payments {
            Some(engine) => engine.on_terminal_response(entry, e_tag, message),
            None => Some(message),
        }
    }

    fn choose_outbound_gift_wrap_kind(&self) -> u16 {
        outbound_gift_wrap_kind(
            self.config.gift_wrap_mode,
            self.server_supports_ephemeral.load(Ordering::Relaxed),
        )
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

/// CEP-22: record an original `_meta.progressToken` value under its
/// stringified form, replacing any stale entry for the same key (free fn so
/// both the transport and [`ClientSendParts`] share one write path).
fn record_original_progress_token(
    originals: &Mutex<LruCache<String, serde_json::Value>>,
    token: &str,
    original: &serde_json::Value,
) {
    let mut originals = match originals.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    originals.push(token.to_string(), original.clone());
}

/// The client transport's outbound send capability, detached from `&self`.
///
/// Cloned handles of everything [`NostrClientTransport::send`] uses, so an
/// in-crate detached task (a payments-engine retry) sends through the FULL
/// production path: negotiation tags, the shared one-shot discovery latch, the
/// `payment_interaction` latch, encryption, CEP-22 fragmentation, and pending
/// registration all behave exactly as an application send does.
#[derive(Clone)]
pub(crate) struct ClientSendParts {
    /// The transport configuration snapshot.
    config: NostrClientTransportConfig,
    /// The server's public key.
    server_pubkey: PublicKey,
    /// The shared relay pool.
    relay_pool: Arc<dyn RelayPoolTrait>,
    /// The shared pending-request correlation store.
    pending_requests: ClientCorrelationStore,
    /// The SHARED one-shot discovery latch (`Arc`: a forked copy would re-send
    /// discovery tags on every engine retry).
    has_sent_discovery_tags: Arc<AtomicBool>,
    /// The shared negotiation state (PMIs + payment-interaction latch).
    negotiation: Arc<Mutex<ClientNegotiationState>>,
    /// Learned server support for ephemeral gift wraps.
    server_supports_ephemeral: Arc<AtomicBool>,
    /// Learned server capabilities (oversized handshake elision).
    discovered_server_capabilities: Arc<Mutex<PeerCapabilities>>,
    /// CEP-22: original progress-token values recorded at send.
    original_progress_tokens: Arc<Mutex<LruCache<String, serde_json::Value>>>,
    /// CEP-22: `accept` handshake waiters.
    accept_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    /// CEP-41: unbound `call_tool_stream` placeholders.
    #[allow(clippy::type_complexity)]
    pending_outbound_open_stream:
        Arc<Mutex<VecDeque<oneshot::Sender<Result<(String, OpenStreamSession)>>>>>,
    /// CEP-41: the reader-session registry.
    open_stream_registry: Arc<AsyncMutex<OpenStreamRegistry>>,
    /// The consumer channel sender (stateless initialize emulation).
    message_tx: Option<tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>>,
}

impl ClientSendParts {
    /// A `BaseTransport` view over the shared relay pool (the same shape the
    /// open-stream publish paths already build on demand).
    fn base(&self) -> BaseTransport {
        BaseTransport {
            relay_pool: Arc::clone(&self.relay_pool),
            encryption_mode: self.config.encryption_mode,
            is_connected: true,
        }
    }

    /// CEP-19: the outbound gift-wrap kind for these parts.
    fn choose_outbound_gift_wrap_kind(&self) -> u16 {
        outbound_gift_wrap_kind(
            self.config.gift_wrap_mode,
            self.server_supports_ephemeral.load(Ordering::Relaxed),
        )
    }

    /// A snapshot of the learned server capabilities.
    fn discovered_server_capabilities(&self) -> PeerCapabilities {
        let guard = match self.discovered_server_capabilities.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard
    }

    /// CEP-22: record the original `_meta.progressToken` value of an outbound
    /// request under its stringified form.
    fn record_original_progress_token(&self, token: &str, original: &serde_json::Value) {
        record_original_progress_token(&self.original_progress_tokens, token, original);
    }

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
        // CEP-41: advertise open-stream support when enabled.
        if self.config.open_stream.enabled {
            tags.push(Tag::custom(
                TagKind::Custom(tags::SUPPORT_OPEN_STREAM.into()),
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

    /// CEP-8: the negotiation tags pending for the next outbound request, paired with
    /// the payment interaction mode they carry (`None` when no `payment_interaction`
    /// tag was included).
    ///
    /// Three separate rules, written out rather than folded into one condition
    /// because they have three different lifetimes:
    ///
    /// 1. `pmi` tags ride *every* negotiation-bearing request and are never latched.
    /// 2. `payment_interaction` is emitted only when the requested mode differs from
    ///    the one last published, so routine invocations carry no tag while a
    ///    mid-session change still reaches the server.
    /// 3. Nothing at all is emitted when neither is configured.
    ///
    /// A requested `transparent` is emitted explicitly: a downgrade intent has to stay
    /// distinguishable from an absent tag (which means "no preference").
    ///
    /// Deliberately synchronous, which is what structurally guarantees the mutex guard
    /// cannot be alive at the `.await` that immediately follows the call in `send`.
    fn get_pending_negotiation_tags(&self) -> (Vec<Tag>, Option<PaymentInteractionMode>) {
        let state = match self.negotiation.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        // Rule 1: never latched.
        let mut tags = pmi_tags(&state.pmis);

        // Rule 2: latched until the requested mode changes. An application send
        // and an engine retry can BOTH compute an unsent mode here and both
        // emit the tag; that is a duplicate same-value upsert, sanctioned by
        // CEP-8's mid-session upsert rule, and deliberately left unserialized.
        let pending_mode = match state.requested {
            Some(mode) if Some(mode) != state.last_sent => Some(mode),
            _ => None,
        };
        if let Some(mode) = pending_mode {
            tags.push(payment_interaction_tag(mode));
        }

        // Rule 3 needs no branch: with no PMIs and no requested mode both of the above
        // produce nothing.
        (tags, pending_mode)
    }

    /// CEP-8: record the payment interaction mode a successful publish carried.
    ///
    /// Keyed on the mode that was actually emitted and threaded here by value rather
    /// than re-read from the state: a second read would be a separate critical
    /// section, and a `set_payment_interaction` landing between the two would latch a
    /// mode that never went on the wire.
    fn mark_payment_interaction_sent(&self, sent: Option<PaymentInteractionMode>) {
        let Some(mode) = sent else {
            return;
        };
        let mut state = match self.negotiation.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        state.last_sent = Some(mode);
    }

    /// Emulate the stateless initialize response toward the local consumer.
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

    /// CEP-41: bind the oldest pending placeholder to `token`, creating its reader
    /// session. A no-op when no placeholder is waiting (an ordinary `tools/call`).
    ///
    /// The original token JSON value is recorded **only when a placeholder is
    /// actually bound** (i.e. for a `call_tool_stream`), so the inbound handler can
    /// restore the numeric type for rmcp's reset-on-progress watcher (an ordinary
    /// `tools/call` with no outbound stream records nothing).
    async fn bind_pending_outbound_open_stream(&self, token: &str, original: &serde_json::Value) {
        let waiter = {
            let mut pending = match self.pending_outbound_open_stream.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            pending.pop_front()
        };
        if let Some(waiter) = waiter {
            self.record_original_progress_token(token, original);
            let result = self
                .create_outbound_open_stream_session(token)
                .await
                .map(|session| (token.to_string(), session));
            let _ = waiter.send(result);
        }
    }

    /// CEP-41: get-or-create the reader session for `token`, injecting the publish
    /// closure used by the consumer `abort` path.
    async fn create_outbound_open_stream_session(&self, token: &str) -> Result<OpenStreamSession> {
        let mut registry = self.open_stream_registry.lock().await;
        if let Some(existing) = registry.get_session(token) {
            return Ok(existing);
        }
        let init = OpenStreamSessionInit {
            publish_frame: Some(self.open_stream_publish_closure()),
            ..Default::default()
        };
        Ok(registry.create_session_with(token, init)?)
    }

    /// CEP-41: build the outbound publish closure for an open-stream session
    /// (publishes a `notifications/progress` frame to the server; the server
    /// correlates by `progressToken`, so no `e` tag is needed).
    fn open_stream_publish_closure(&self) -> PublishFrame {
        let relay_pool = Arc::clone(&self.relay_pool);
        let encryption_mode = self.config.encryption_mode;
        let server_pubkey = self.server_pubkey;
        let gift_wrap_kind = self.choose_outbound_gift_wrap_kind();
        Arc::new(move |notification: JsonRpcNotification| {
            let relay_pool = Arc::clone(&relay_pool);
            Box::pin(async move {
                let base = BaseTransport {
                    relay_pool,
                    encryption_mode,
                    is_connected: true,
                };
                let tags = BaseTransport::create_recipient_tags(&server_pubkey);
                base.send_mcp_message(
                    &JsonRpcMessage::Notification(notification),
                    &server_pubkey,
                    CTXVM_MESSAGES_KIND,
                    tags,
                    None,
                    Some(gift_wrap_kind),
                )
                .await
            })
        })
    }

    /// Send a JSON-RPC message to the server through the full production path.
    /// This IS the body of [`NostrClientTransport::send`]; the `&self` method
    /// is a thin wrapper over these parts.
    pub(crate) async fn send(&self, message: &JsonRpcMessage) -> Result<()> {
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

        // CEP-41: bind a pending `call_tool_stream` placeholder to this request's
        // SDK-stamped progress token, synchronously before publish (mirrors TS), and
        // record the token's original JSON type. Inbound stream progress is then
        // forwarded with that type restored so rmcp's reset-on-progress watcher,
        // keyed by the numeric token rmcp stamped, actually fires.
        if is_request && self.config.open_stream.enabled {
            if let JsonRpcMessage::Request(req) = message {
                if req.method == "tools/call" {
                    if let Some(original) = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                    {
                        if let Some(token) = progress_token_string(original) {
                            self.bind_pending_outbound_open_stream(&token, original)
                                .await;
                        }
                    }
                }
            }
        }

        let base_tags = BaseTransport::create_recipient_tags(&self.server_pubkey);
        let discovery_tags = if is_request {
            self.get_pending_client_discovery_tags()
        } else {
            vec![]
        };
        // CEP-8: negotiation tags ride requests only, mirroring the discovery gate
        // above. A notification therefore neither emits them nor burns the latch.
        let (negotiation_tags, pending_payment_interaction) = if is_request {
            self.get_pending_negotiation_tags()
        } else {
            (vec![], None)
        };
        let tags =
            BaseTransport::compose_outbound_tags(&base_tags, &discovery_tags, &negotiation_tags);
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
        // on the published byte size, not the raw payload, which is what actually
        // grows under JSON escaping and gift-wrap encryption (mirrors TS
        // `measurePublishedMcpMessageSize`). The raw serialized length is a cheap
        // lower bound: when it already meets the threshold the message is
        // conclusively oversized and we fragment without building a single event;
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
                        pending_payment_interaction,
                    )
                    .await;
            }
            // Borderline: a sub-threshold payload can still cross the threshold once
            // signed, JSON-escaped, and (when enabled) gift-wrapped. Build the single
            // event once, measure its real published size, and reuse it if it fits.
            match self
                .base()
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
                                pending_payment_interaction,
                            )
                            .await;
                    }
                    return self
                        .publish_single_event(
                            message,
                            event_id,
                            publishable_event,
                            discovery_sent,
                            pending_payment_interaction,
                        )
                        .await;
                }
                Err(error) => {
                    // Could not build even one event (e.g. NIP-44 plaintext overflow
                    // from an escape-heavy payload): it cannot be sent as a single
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
                            pending_payment_interaction,
                        )
                        .await;
                }
            }
        }

        // Single-event path: not oversized-eligible (notification, feature disabled,
        // or no progressToken).
        let (event_id, publishable_event) = self
            .base()
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

        self.publish_single_event(
            message,
            event_id,
            publishable_event,
            discovery_sent,
            pending_payment_interaction,
        )
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
        pending_payment_interaction: Option<PaymentInteractionMode>,
    ) -> Result<()> {
        if let JsonRpcMessage::Request(ref req) = message {
            let is_initialize = req.method == INITIALIZE_METHOD;
            self.pending_requests
                .register(
                    event_id.to_hex(),
                    req.id.clone(),
                    is_initialize,
                    request_progress_token(req),
                )
                .await;
        }

        if let Err(error) = self.relay_pool.publish_event(&publishable_event).await {
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
        self.mark_payment_interaction_sent(pending_payment_interaction);

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
    /// Builds `start -> chunks -> end` frames, registers an `accept` waiter before
    /// publishing `start` when the server's support is not yet known, drives the
    /// [`send_oversized_transfer`] sequencer, and registers the pending request
    /// against the **end** frame's event id (the value the server correlates its
    /// response to). One-shot discovery tags ride the `start` frame only.
    #[allow(clippy::too_many_arguments)]
    async fn send_oversized_request(
        &self,
        message: &JsonRpcMessage,
        content: &str,
        token: &str,
        base_tags: Vec<Tag>,
        start_tags: Vec<Tag>,
        discovery_sent: bool,
        pending_payment_interaction: Option<PaymentInteractionMode>,
    ) -> Result<()> {
        // The handshake is required until the server is known to support oversized
        // transfer; once learned, chunks start immediately (no accept slot).
        let needs_accept = !self
            .discovered_server_capabilities()
            .supports_oversized_transfer;

        let gift_wrap_kind = self.choose_outbound_gift_wrap_kind();
        let owned_base = self.base();
        // Effective encryption for these frames (the publish closure passes `None`,
        // letting `should_encrypt` decide from the mode; resolve the same boolean
        // here so the sizing measurement matches the real published frames).
        let is_encrypted = owned_base.should_encrypt(CTXVM_MESSAGES_KIND, None);

        // CEP-22: derive a per-chunk payload budget so every published frame stays
        // under the threshold even after the JSON-RPC envelope, signature, and
        // (when encrypted) gift-wrap expansion. Mirrors TS `resolveSafeOversizedChunkSize`.
        // Continuation (chunk) frames carry the bare recipient `p`-tags (`base_tags`),
        // so size against those.
        let chunk_size = resolve_safe_chunk_size(
            self.config.oversized_transfer.chunk_size,
            &owned_base,
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
        let base = &owned_base;
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
                .register(
                    end_id.to_hex(),
                    req.id.clone(),
                    is_initialize,
                    request_progress_token(req),
                )
                .await;
        }

        // Flip the one-shot discovery flag after a successful transfer.
        if discovery_sent {
            self.has_sent_discovery_tags.store(true, Ordering::Relaxed);
        }
        // The negotiation tags rode the `start` frame, so the latch flips here too.
        // Missing this site would re-send `payment_interaction` on every later request
        // once a first request happened to be oversized.
        self.mark_payment_interaction_sent(pending_payment_interaction);

        tracing::debug!(
            target: LOG_TARGET,
            end_event_id = %end_id.to_hex(),
            method = ?message.method(),
            "Sent oversized client request"
        );
        Ok(())
    }
}

#[inline]
fn is_gift_wrap_kind(kind: &Kind) -> bool {
    *kind == Kind::Custom(GIFT_WRAP_KIND) || *kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)
}

/// CEP-19: the outbound gift-wrap kind for the client (free fn so the static
/// event-loop / sweep contexts can resolve it without `&self`).
#[inline]
fn outbound_gift_wrap_kind(mode: GiftWrapMode, server_supports_ephemeral: bool) -> u16 {
    match mode {
        GiftWrapMode::Persistent => GIFT_WRAP_KIND,
        GiftWrapMode::Ephemeral => EPHEMERAL_GIFT_WRAP_KIND,
        GiftWrapMode::Optional => {
            if server_supports_ephemeral {
                EPHEMERAL_GIFT_WRAP_KIND
            } else {
                GIFT_WRAP_KIND
            }
        }
    }
}

/// CEP-8: the original `_meta.progressToken` JSON value of an outbound request,
/// recorded into its pending entry at registration. The value is kept exactly as
/// sent (`Number(5)` and `String("5")` stay distinct) because rmcp's progress
/// watcher is keyed by the token's exact JSON type.
fn request_progress_token(req: &JsonRpcRequest) -> Option<serde_json::Value> {
    req.params
        .as_ref()?
        .get("_meta")?
        .get("progressToken")
        .cloned()
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
            has_sent_discovery_tags: Arc::new(AtomicBool::new(false)),
            discovered_server_capabilities: Arc::new(Mutex::new(PeerCapabilities::default())),
            server_initialize_event: Arc::new(Mutex::new(None)),
            server_supports_ephemeral: Arc::new(AtomicBool::new(false)),
            seen_gift_wrap_ids: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(10).unwrap()))),
            negotiation: Arc::new(Mutex::new(ClientNegotiationState::default())),
            oversized_receiver: Arc::new(Mutex::new(OversizedTransferReceiver::new())),
            accept_waiters: Arc::new(Mutex::new(HashMap::new())),
            original_progress_tokens: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(10).unwrap(),
            ))),
            open_stream_registry: Arc::new(AsyncMutex::new(OpenStreamRegistry::new())),
            pending_outbound_open_stream: Arc::new(Mutex::new(VecDeque::new())),
            open_stream_control_progress: Arc::new(AtomicU64::new(0)),
            open_stream_bind_lock: Arc::new(AsyncMutex::new(())),
            message_tx: Some(tokio::sync::mpsc::unbounded_channel().0),
            message_rx: None,
            cancellation_token: CancellationToken::new(),
            event_loop_handle: None,
            client_payments: None,
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
        // The oversized tag (default-on) is pushed last; open-stream is opt-in.
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
        // No encryption tags; the default-on oversized tag remains (open-stream is opt-in).
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
    fn client_capability_tags_open_stream_gate() {
        // Open-stream is opt-in: absent by default.
        let mut t = make_transport_for_tags(EncryptionMode::Disabled, GiftWrapMode::Optional);
        assert!(
            !tag_names(&t.get_client_capability_tags())
                .contains(&"support_open_stream".to_string()),
            "open-stream tag must be absent by default (opt-in)"
        );
        // Enabling it advertises the single-element tag.
        t.config.open_stream = OpenStreamConfig::enabled();
        assert!(
            tag_names(&t.get_client_capability_tags()).contains(&"support_open_stream".to_string()),
            "open-stream tag must be advertised when enabled"
        );
    }

    #[test]
    fn client_learn_server_discovery_learns_open_stream() {
        let caps = Mutex::new(PeerCapabilities::default());
        let init = Mutex::new(None);
        let negotiation = Mutex::new(ClientNegotiationState::default());
        let event = make_event_with_tags(&[&["support_open_stream"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &event);
        assert!(caps.lock().unwrap().supports_open_stream);
    }

    // ── CEP-8 client negotiation ─────────────────────────────────

    /// Pins the "nothing configured" rule. This is a claim about the getter only; the
    /// matching claim about the *wire* belongs to the integration test
    /// `unconfigured_client_emits_the_same_tags_as_before`, which asserts a published
    /// event's whole tag list.
    #[test]
    fn negotiation_tags_empty_when_nothing_configured() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        let (tags, mode) = t.get_pending_negotiation_tags();
        assert!(tags.is_empty(), "no PMIs and no mode means no tags");
        assert_eq!(mode, None, "and nothing for the latch to record");
    }

    /// A requested `transparent` is emitted explicitly: a downgrade intent must stay
    /// distinguishable from an absent tag.
    #[test]
    fn negotiation_tags_emit_transparent_explicitly() {
        let t = make_transport_for_tags(EncryptionMode::Optional, GiftWrapMode::Optional);
        t.set_payment_interaction(PaymentInteractionMode::Transparent);
        let (tags, mode) = t.get_pending_negotiation_tags();
        assert_eq!(
            tags.iter().map(|t| t.clone().to_vec()).collect::<Vec<_>>(),
            vec![vec![
                tags::PAYMENT_INTERACTION.to_string(),
                "transparent".to_string()
            ]],
        );
        assert_eq!(mode, Some(PaymentInteractionMode::Transparent));
    }

    /// An unrecognized value leaves a previously recorded mode intact.
    #[test]
    fn learner_ignores_an_unknown_mode_value() {
        let caps = Mutex::new(PeerCapabilities::default());
        let init = Mutex::new(None);
        let negotiation = Mutex::new(ClientNegotiationState {
            requested: Some(PaymentInteractionMode::ExplicitGating),
            effective: Some(PaymentInteractionMode::ExplicitGating),
            ..Default::default()
        });
        let event = make_event_with_tags(&[&[tags::PAYMENT_INTERACTION, "bogus"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &event);
        assert_eq!(
            negotiation.lock().unwrap().effective,
            Some(PaymentInteractionMode::ExplicitGating),
            "an unparseable value must not clobber the recorded mode"
        );
    }

    /// Two conflicting tags: the first wins, matching both SDKs' readers.
    #[test]
    fn learner_takes_the_first_tag_of_many() {
        let caps = Mutex::new(PeerCapabilities::default());
        let init = Mutex::new(None);
        let negotiation = Mutex::new(ClientNegotiationState {
            requested: Some(PaymentInteractionMode::ExplicitGating),
            ..Default::default()
        });
        let event = make_event_with_tags(&[
            &[tags::PAYMENT_INTERACTION, "transparent"],
            &[tags::PAYMENT_INTERACTION, "explicit_gating"],
        ]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &event);
        assert_eq!(
            negotiation.lock().unwrap().effective,
            Some(PaymentInteractionMode::Transparent),
            "the first payment_interaction tag wins"
        );
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

    // ── CEP-8 consumption rule + payment touch + token capture ──

    /// Shared state for driving raw relay notifications through
    /// `handle_notification` without a live event loop.
    struct InboundFixture {
        pending: ClientCorrelationStore,
        server_keys: Keys,
        tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
        rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
        relay_pool: Arc<dyn RelayPoolTrait>,
    }

    impl InboundFixture {
        fn new() -> Self {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            Self {
                pending: ClientCorrelationStore::new(),
                server_keys: Keys::generate(),
                tx,
                rx,
                relay_pool: Arc::new(crate::relay::mock::MockRelayPool::new()),
            }
        }

        /// Deliver one server-signed plaintext event with `content` and an
        /// optional correlation `e` tag through the inbound handler.
        async fn deliver(&self, content: &str, e_tag: Option<&EventId>) {
            let mut builder = EventBuilder::new(Kind::Custom(CTXVM_MESSAGES_KIND), content);
            if let Some(id) = e_tag {
                builder = builder.tag(Tag::event(*id));
            }
            let event = builder
                .sign_with_keys(&self.server_keys)
                .expect("sign inbound event");
            let notification = RelayPoolNotification::Event {
                relay_url: RelayUrl::parse("wss://mock.relay").expect("hardcoded URL"),
                subscription_id: SubscriptionId::generate(),
                event: Box::new(event),
            };
            NostrClientTransport::handle_notification(
                &notification,
                &self.pending,
                self.server_keys.public_key(),
                &self.tx,
                EncryptionMode::Optional,
                GiftWrapMode::Optional,
                &Arc::new(Mutex::new(PeerCapabilities::default())),
                &Arc::new(Mutex::new(None)),
                &Arc::new(Mutex::new(ClientNegotiationState::default())),
                &Arc::new(AtomicBool::new(false)),
                &Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(10).unwrap()))),
                &Arc::new(Mutex::new(OversizedTransferReceiver::new())),
                &Arc::new(Mutex::new(HashMap::new())),
                &Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(10).unwrap()))),
                &Arc::new(AsyncMutex::new(OpenStreamRegistry::new())),
                &Arc::new(AtomicU64::new(0)),
                false,
                &None,
                &self.relay_pool,
            )
            .await;
        }
    }

    fn notification_json(method: &str, params: serde_json::Value) -> String {
        serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
    }

    fn response_json(id: &str) -> String {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [] } }).to_string()
    }

    /// A correlated payment notification is forwarded WITHOUT consuming the
    /// pending entry; the response that follows is then still delivered.
    #[tokio::test]
    async fn correlated_notification_leaves_the_pending_entry() {
        let mut fx = InboundFixture::new();
        let request_event = EventId::all_zeros();
        fx.pending
            .register(
                request_event.to_hex(),
                serde_json::json!("req-1"),
                false,
                None,
            )
            .await;

        fx.deliver(
            &notification_json(
                "notifications/payment_accepted",
                serde_json::json!({ "amount": 21, "pmi": "fake" }),
            ),
            Some(&request_event),
        )
        .await;
        assert!(
            fx.pending.contains(&request_event.to_hex()).await,
            "a correlated notification must not consume the correlation entry"
        );
        let forwarded = fx.rx.try_recv().expect("the notification is forwarded");
        assert!(matches!(forwarded, JsonRpcMessage::Notification(_)));

        fx.deliver(&response_json("req-1"), Some(&request_event))
            .await;
        assert!(
            !fx.pending.contains(&request_event.to_hex()).await,
            "the response consumes the entry"
        );
        let delivered = fx.rx.try_recv().expect("the response is delivered");
        assert!(
            matches!(delivered, JsonRpcMessage::Response(_)),
            "the real response must survive the earlier correlated notification"
        );
    }

    /// The consumption rule is type-based, not method-based: a correlated custom
    /// notification leaves the entry alone too, and the response still delivers.
    #[tokio::test]
    async fn non_payment_correlated_notification_also_leaves_it() {
        let mut fx = InboundFixture::new();
        let request_event = EventId::all_zeros();
        fx.pending
            .register(request_event.to_hex(), serde_json::json!(3), false, None)
            .await;

        fx.deliver(
            &notification_json("notifications/custom_event", serde_json::json!({ "k": 1 })),
            Some(&request_event),
        )
        .await;
        assert!(fx.pending.contains(&request_event.to_hex()).await);
        assert!(fx.rx.try_recv().is_ok(), "the custom notification forwards");

        fx.deliver(&response_json("3"), Some(&request_event)).await;
        assert!(!fx.pending.contains(&request_event.to_hex()).await);
        assert!(fx.rx.try_recv().is_ok(), "the response delivers");
    }

    /// A correlated `payment_required` one-shot-touches the entry, so a sweep
    /// that would evict an untouched sibling leaves the paying request alive.
    #[tokio::test]
    async fn payment_required_touch_refreshes_the_entry_against_the_sweep() {
        let fx = InboundFixture::new();
        let paying = EventId::all_zeros();
        let idle = EventId::from_slice(&[7u8; 32]).expect("32 bytes");
        fx.pending
            .register(paying.to_hex(), serde_json::json!(1), false, None)
            .await;
        fx.pending
            .register(idle.to_hex(), serde_json::json!(2), false, None)
            .await;

        tokio::time::sleep(Duration::from_millis(30)).await;
        fx.deliver(
            &notification_json(
                PAYMENT_REQUIRED_METHOD,
                serde_json::json!({ "amount": 21, "pay_req": "inv", "pmi": "fake" }),
            ),
            Some(&paying),
        )
        .await;

        let swept = fx.pending.sweep_expired(Duration::from_millis(20)).await;
        assert_eq!(swept, 1, "only the untouched entry expires");
        assert!(
            fx.pending.contains(&paying.to_hex()).await,
            "the touched paying entry must survive the sweep"
        );
        assert!(!fx.pending.contains(&idle.to_hex()).await);
    }

    /// `send()` records the request's original `_meta.progressToken` JSON value
    /// into the pending entry: numbers stay numbers, strings stay strings, and a
    /// request without `_meta` records nothing.
    #[tokio::test]
    async fn send_records_the_progress_token_into_the_pending_entry() {
        let pool = Arc::new(crate::relay::mock::MockRelayPool::new());
        let mut t = make_transport_for_tags(EncryptionMode::Disabled, GiftWrapMode::Optional);
        t.base.relay_pool = Arc::clone(&pool) as Arc<dyn RelayPoolTrait>;

        let send_and_take_token = |req: JsonRpcMessage| {
            let t = &t;
            let pool = Arc::clone(&pool);
            async move {
                t.send(&req).await.expect("send");
                let event_id = pool
                    .stored_events()
                    .await
                    .last()
                    .expect("the send published an event")
                    .id
                    .to_hex();
                t.pending_requests
                    .remove(&event_id)
                    .await
                    .expect("the send registered a pending entry")
                    .progress_token
            }
        };

        let request = |id: i64, params: serde_json::Value| {
            JsonRpcMessage::Request(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(id),
                method: "tools/call".to_string(),
                params: Some(params),
            })
        };

        let numeric = send_and_take_token(request(
            1,
            serde_json::json!({ "_meta": { "progressToken": 7 } }),
        ))
        .await;
        assert_eq!(
            numeric,
            Some(serde_json::json!(7)),
            "a numeric token must be recorded as a JSON number"
        );

        let string = send_and_take_token(request(
            2,
            serde_json::json!({ "_meta": { "progressToken": "7" } }),
        ))
        .await;
        assert_eq!(
            string,
            Some(serde_json::json!("7")),
            "a string token must stay a JSON string, never parsed to a number"
        );

        let bare = send_and_take_token(request(3, serde_json::json!({ "name": "t" }))).await;
        assert_eq!(bare, None, "no _meta means no recorded token");
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
        let negotiation = Mutex::new(ClientNegotiationState::default());
        let event = make_event_with_tags(&[&["support_encryption"], &["name", "TestServer"]]);

        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &event);

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
        let negotiation = Mutex::new(ClientNegotiationState::default());

        let event1 = make_event_with_tags(&[&["support_encryption"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &event1);

        // Second event with different caps does NOT downgrade
        let event2 = make_event_with_tags(&[&["support_encryption_ephemeral"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &event2);

        let c = caps.lock().unwrap();
        assert!(c.supports_encryption, "must not downgrade");
        assert!(c.supports_ephemeral_encryption, "must learn new cap");
    }

    #[test]
    fn client_baseline_not_replaced_on_later_events() {
        let caps = Mutex::new(PeerCapabilities::default());
        let init = Mutex::new(None);
        let negotiation = Mutex::new(ClientNegotiationState::default());

        let event1 = make_event_with_tags(&[&["support_encryption"], &["name", "First"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &event1);
        let first_id = event1.id;

        let event2 =
            make_event_with_tags(&[&["support_encryption_ephemeral"], &["name", "Second"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &event2);

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
        let negotiation = Mutex::new(ClientNegotiationState::default());

        // First discovery tags arrive on a non-initialize event (e.g. a notification).
        let baseline = make_event_with_tags(&[&["support_encryption"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &baseline);
        assert_eq!(init.lock().unwrap().as_ref().unwrap().id, baseline.id);

        // A later event carries a full InitializeResult → baseline is upgraded.
        let init_event = make_event_with_content_and_tags(
            &initialize_result_content(),
            &[&["support_encryption"]],
        );
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &init_event);
        assert_eq!(
            init.lock().unwrap().as_ref().unwrap().id,
            init_event.id,
            "baseline must upgrade to the initialize-result event"
        );

        // A still-later non-initialize event must NOT downgrade the baseline.
        let later = make_event_with_tags(&[&["support_encryption_ephemeral"]]);
        NostrClientTransport::learn_server_discovery(&caps, &init, &negotiation, &later);
        assert_eq!(
            init.lock().unwrap().as_ref().unwrap().id,
            init_event.id,
            "baseline must not downgrade away from the initialize result"
        );
    }
}
