//! CEP-8 client-side payments: the engine behind `with_client_payments`.
//!
//! The engine is integrated into the client transport's inbound path rather
//! than wrapping the transport: the proxy owns the transport by value, inbound
//! traffic exits through a plain channel that strips event-id metadata, and the
//! correlation store the payment flows depend on lives inside the transport.
//! The transport invokes the engine at fixed hook points (outbound request
//! caching, correlated payment notifications, and terminal responses); the
//! engine owns every payment decision and all payment state.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::FutureExt;
use lru::LruCache;
use tokio_util::sync::CancellationToken;

use crate::core::types::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    PaymentInteractionMode,
};
use crate::payments::constants::{
    DEFAULT_PAYMENT_TTL_MS, DEFAULT_SYNTHETIC_PROGRESS_INTERVAL_MS, PAYMENT_ACCEPTED_METHOD,
    PAYMENT_PENDING_ERROR_CODE, PAYMENT_REJECTED_METHOD, PAYMENT_REQUIRED_ERROR_CODE,
    PAYMENT_REQUIRED_METHOD,
};
use crate::payments::errors::PaymentError;
use crate::payments::traits::PaymentHandler;
use crate::payments::types::{
    PaymentHandlerRequest, PaymentOption, PaymentPendingErrorData, PaymentRejectedParams,
    PaymentRequiredErrorData, PaymentRequiredParams,
};
use crate::transport::client::correlation_store::PendingRequest;
use crate::transport::client::{ClientCorrelationStore, ClientSendParts, NostrClientTransport};

const LOG_TARGET: &str = "contextvm_sdk::payments::client_payments";

/// Capacity of the raw-request cache backing explicit-gating retries (the
/// reference implementation's value).
const RAW_REQUEST_CACHE_CAPACITY: usize = 1000;

/// Default cap on `-32043` retries before the raw error is surfaced.
const DEFAULT_MAX_PENDING_RETRIES: u32 = 10;

// ── Callback contracts ──────────────────────────────────────────────

/// Async policy gate for transparent auto-pay, evaluated before the handler's
/// own `can_handle`. Returning `false` declines the payment: the engine stops
/// the request's keep-alives and synthesizes a `-32000` decline toward the
/// local consumer. The request is passed by value: it is one small owned
/// struct, and the policy usually moves fields into its async block.
pub type PaymentPolicyFn = dyn Fn(PaymentHandlerRequest) -> BoxFuture<'static, bool> + Send + Sync;

/// Handler for explicit-gating `-32042 Payment Required` errors. Pay one of
/// the offered options out of band (or refuse), then report the outcome:
/// `paid: true` makes the engine retry the original request byte-for-byte;
/// `paid: false` surfaces a synthesized `-32042` carrying `reason`; an `Err`
/// surfaces a synthesized `-32042` with `type: "payment_handler_error"`.
pub type OnPaymentRequiredFn = dyn Fn(PaymentRequiredCallbackParams) -> BoxFuture<'static, Result<PaymentApproval, PaymentError>>
    + Send
    + Sync;

/// Outcome reported by an [`OnPaymentRequiredFn`] callback.
#[derive(Debug, Clone)]
pub struct PaymentApproval {
    /// Whether the payment was made; `true` triggers the automatic retry.
    pub paid: bool,
    /// Optional reason carried into the synthesized error when `paid` is
    /// `false` (defaults to `"user_cancelled"`).
    pub reason: Option<String>,
}

/// Input to an [`OnPaymentRequiredFn`] callback.
#[derive(Debug, Clone)]
pub struct PaymentRequiredCallbackParams {
    /// The payment options offered by the server (at least one).
    pub options: Vec<PaymentOption>,
    /// Optional human-readable payment instructions from the server.
    pub instructions: Option<String>,
    /// The original request being gated, exactly as this client sent it.
    pub original_request: JsonRpcRequest,
}

// ── Options ─────────────────────────────────────────────────────────

/// Configuration for [`with_client_payments`](crate::payments::client_payments).
///
/// Mirrors the reference implementation's client options object, with
/// durations as [`Duration`] rather than millisecond numbers.
#[derive(Clone)]
#[non_exhaustive]
pub struct ClientPaymentsOptions {
    /// Payment handlers for in-band (programmatic) payment, one per PMI.
    /// Their PMIs are advertised to the server in order, REPLACING any list
    /// seeded through the transport config. Leave empty for a PMI-agnostic
    /// client that pays out of band: no PMIs are advertised, the server's
    /// `payment_required` is forwarded to the application, and synthetic
    /// progress keeps the request alive while it settles externally.
    pub handlers: Vec<Arc<dyn PaymentHandler>>,
    /// Interval between synthetic-progress heartbeats. One beat is also
    /// emitted immediately when `payment_required` arrives, so the MCP
    /// timeout resets as soon as the payment flow begins. Default 30 s.
    pub synthetic_progress_interval: Duration,
    /// Keep-alive duration when `payment_required` carries no `ttl`. Mirrors
    /// the server-side default so the client waits at least as long as the
    /// server will. Default 300 s.
    pub default_payment_ttl: Duration,
    /// Optional async policy gate evaluated before the handler's `can_handle`.
    /// `None` approves every payment (the handlers still gate themselves).
    pub payment_policy: Option<Arc<PaymentPolicyFn>>,
    /// Requested payment interaction mode. `Some` overrides the transport
    /// config's seed at registration; `None` leaves the config's seed alone.
    pub payment_interaction: Option<PaymentInteractionMode>,
    /// Maximum number of `-32043 Payment Pending` retries before the raw
    /// error surfaces to the consumer. Counted as retries with the cap
    /// checked before each increment, so the default of 10 allows up to 11
    /// total sends.
    pub max_pending_retries: u32,
    /// Handler for explicit-gating `-32042` errors. `None` (the transparent
    /// client's shape) forwards the raw error to the consumer.
    pub on_payment_required: Option<Arc<OnPaymentRequiredFn>>,
}

impl ClientPaymentsOptions {
    /// Options with no handlers, no callbacks, and the reference defaults
    /// (30 s heartbeat interval, 300 s default TTL, 10 pending retries).
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
            synthetic_progress_interval: Duration::from_millis(
                DEFAULT_SYNTHETIC_PROGRESS_INTERVAL_MS,
            ),
            default_payment_ttl: Duration::from_millis(DEFAULT_PAYMENT_TTL_MS),
            payment_policy: None,
            payment_interaction: None,
            max_pending_retries: DEFAULT_MAX_PENDING_RETRIES,
            on_payment_required: None,
        }
    }

    /// Set the in-band payment handlers (their PMIs are advertised in order).
    pub fn with_handlers(mut self, handlers: Vec<Arc<dyn PaymentHandler>>) -> Self {
        self.handlers = handlers;
        self
    }

    /// Set the synthetic-progress heartbeat interval.
    pub fn with_synthetic_progress_interval(mut self, interval: Duration) -> Self {
        self.synthetic_progress_interval = interval;
        self
    }

    /// Set the keep-alive duration used when `payment_required` has no `ttl`.
    pub fn with_default_payment_ttl(mut self, ttl: Duration) -> Self {
        self.default_payment_ttl = ttl;
        self
    }

    /// Set the async payment policy gate.
    pub fn with_payment_policy(mut self, policy: Arc<PaymentPolicyFn>) -> Self {
        self.payment_policy = Some(policy);
        self
    }

    /// Request a payment interaction mode for the session.
    pub fn with_payment_interaction(mut self, mode: PaymentInteractionMode) -> Self {
        self.payment_interaction = Some(mode);
        self
    }

    /// Set the cap on `-32043` retries.
    pub fn with_max_pending_retries(mut self, max: u32) -> Self {
        self.max_pending_retries = max;
        self
    }

    /// Set the explicit-gating `-32042` handler.
    pub fn with_on_payment_required(mut self, callback: Arc<OnPaymentRequiredFn>) -> Self {
        self.on_payment_required = Some(callback);
        self
    }
}

impl Default for ClientPaymentsOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ClientPaymentsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientPaymentsOptions")
            .field(
                "handlers",
                &self
                    .handlers
                    .iter()
                    .map(|h| h.pmi().to_string())
                    .collect::<Vec<_>>(),
            )
            .field(
                "synthetic_progress_interval",
                &self.synthetic_progress_interval,
            )
            .field("default_payment_ttl", &self.default_payment_ttl)
            .field(
                "payment_policy",
                &self.payment_policy.as_ref().map(|_| ".."),
            )
            .field("payment_interaction", &self.payment_interaction)
            .field("max_pending_retries", &self.max_pending_retries)
            .field(
                "on_payment_required",
                &self.on_payment_required.as_ref().map(|_| ".."),
            )
            .finish()
    }
}

// ── Registration ────────────────────────────────────────────────────

/// Attach CEP-8 client payments to a [`NostrClientTransport`].
///
/// This is the client peer of
/// [`with_server_payments`](crate::payments::with_server_payments) and the one
/// production entry point for the client payment surface. It advertises the
/// handlers' PMIs, applies a requested payment-interaction mode, and installs
/// the payment engine into the transport's inbound path, which then drives
/// both lifecycles: transparent auto-pay (policy-gated wallet handlers,
/// synthetic-progress keep-alives, decline and rejection synthesis) and
/// explicit-gating interception (`-32042` pay-and-retry through the
/// `on_payment_required` callback, `-32043` retry with capped backoff).
///
/// # Registration contract
///
/// Call this exactly once, after constructing the transport and before
/// [`start`](NostrClientTransport::start). All three misuses are refused with
/// an error before any state changes: a started transport's event loop no
/// longer observes the engine, so a late registration would advertise PMIs it
/// never honors and leak raw gating errors to the consumer; a closed
/// transport is not restartable, so a post-close registration could only
/// mutate negotiation state on a dead transport; and a second registration is
/// refused outright rather than silently replacing (or doubling) the payment
/// surface of the first.
///
/// # PMI advertisement replaces the configured list
///
/// The handlers' PMIs are advertised in registration order and REPLACE any
/// list seeded through
/// [`NostrClientTransportConfig::pmis`](crate::transport::client::NostrClientTransportConfig),
/// mirroring the reference implementation. The supported shape is
/// handlers-own-the-PMIs: a client with no handlers advertises no PMIs (the
/// out-of-band shape), even when the config seeded some.
/// [`ClientPaymentsOptions::payment_interaction`] behaves differently on
/// purpose: `None` leaves the config's seed alone.
///
/// # Threat model, for auto-pay operators
///
/// Every inbound byte the engine reacts to is an untrusted server's. The
/// transport already drops, before the engine runs, events not signed by the
/// configured server and correlated messages that match no live pending
/// request, so a third party cannot address this client and a replayed or
/// forged invoice for a request that is not in flight never reaches a
/// handler. What no gate can judge is a FRESH offer from the real server for
/// a request that is still pending: amounts, TTLs, and repeat offers are
/// exactly what [`ClientPaymentsOptions::payment_policy`] exists to gate, and
/// an auto-pay deployment should treat that callback as its spending limit.
/// Two adjacent behaviors are deliberate, matching the reference
/// implementation: the in-flight dedup is keyed by the offer's `pay_req` and
/// released when the handler settles, so a distinct re-offer for the same
/// still-pending request is honored (policy-gated); and a session in which
/// the server ACCEPTED explicit gating still auto-pays transparent offers
/// (policy-gated), because only the client's requested mode being refused
/// disables transparent satisfaction.
///
/// # Synthetic progress is indistinguishable from real progress
///
/// The keep-alive heartbeat fabricates `notifications/progress` beats with
/// `progress: 0` toward the local consumer while a payment settles. A
/// consumer cannot tell a fabricated beat from a real server-sent one; the
/// reference implementation behaves identically, and any marker would be a
/// consumer-visible divergence. The fabrication window is bounded by the
/// offer's `ttl` (or [`ClientPaymentsOptions::default_payment_ttl`]).
///
/// # Errors
///
/// Fails without mutating the transport when the transport is already
/// started, already closed, or already has client payments registered.
pub fn with_client_payments(
    transport: &mut NostrClientTransport,
    options: ClientPaymentsOptions,
) -> crate::Result<()> {
    // All three guards run before any mutation, so a failed call leaves the
    // transport exactly as it was and the caller can correct and retry.
    if transport.is_started() {
        return Err(crate::Error::Other(
            "with_client_payments must be called before start()".to_string(),
        ));
    }
    if transport.is_closed() {
        return Err(crate::Error::Other(
            "with_client_payments cannot register on a closed transport".to_string(),
        ));
    }
    if transport.client_payments_installed() {
        return Err(crate::Error::Other(
            "client payments are already registered on this transport; \
             with_client_payments registers once and owns the payment surface"
                .to_string(),
        ));
    }

    // Advertise the handlers' PMIs in registration order (REPLACING any
    // config-seeded list), and apply a requested mode only when one is set.
    let pmis: Vec<String> = options
        .handlers
        .iter()
        .map(|handler| handler.pmi().to_string())
        .collect();
    transport.set_client_pmis(pmis);
    if let Some(mode) = options.payment_interaction {
        transport.set_payment_interaction(mode);
    }

    let message_tx = transport
        .consumer_sender()
        .expect("guarded above: the transport is not closed");
    let engine = ClientPaymentsEngine::new(
        &options,
        message_tx,
        transport.send_parts(),
        transport.correlation_store(),
        transport.correlation_timeout(),
    );
    transport.install_client_payments(engine);
    Ok(())
}

// ── Engine internals ────────────────────────────────────────────────

/// Whether `method` is one of the three CEP-8 payment notifications the
/// engine's inbound hook handles.
pub(crate) fn is_payment_notification_method(method: &str) -> bool {
    matches!(
        method,
        PAYMENT_REQUIRED_METHOD | PAYMENT_ACCEPTED_METHOD | PAYMENT_REJECTED_METHOD
    )
}

/// One cached outbound request, retry-capable: the exact original
/// [`JsonRpcRequest`] (including `_meta`) plus this request's `-32043` retry
/// count. Keeping the counter inside the entry bounds both together under the
/// cache's LRU eviction; a separate counter map would leak counters for
/// requests that never see a terminal response.
struct CachedRequest {
    request: JsonRpcRequest,
    pending_retries: u32,
}

/// One live synthetic-progress heartbeat.
struct HeartbeatEntry {
    /// Hard stop, aged on `std::time::Instant`.
    stop_at: Instant,
    /// The ORIGINAL token JSON value recorded at send; beats carry it
    /// verbatim (rmcp's progress watcher is keyed by exact JSON type).
    token: serde_json::Value,
}

/// Heartbeat entries plus the shared scheduler's running flag, under one lock
/// so start/stop transitions are atomic against the scheduler's own emptiness
/// check.
#[derive(Default)]
struct HeartbeatState {
    entries: HashMap<String, HeartbeatEntry>,
    scheduler_running: bool,
}

/// The shared cache key over a JSON-RPC request id, used at cache write (the
/// outbound request's own id) and at lookup (the consumed correlation entry's
/// `original_id`). Deriving the lookup key from the correlation entry rather
/// than the arriving error's wire id is what keeps this client working against
/// BOTH server flavors: this SDK's servers answer gating errors with the
/// original inner id, while the reference servers answer with the rewritten
/// event id.
fn cache_key(id: &serde_json::Value) -> String {
    id.to_string()
}

/// The map key for a heartbeat token (its serialized JSON form, which keeps
/// `5` and `"5"` distinct).
fn token_key(token: &serde_json::Value) -> String {
    token.to_string()
}

/// The `-32043` retry delay: the server's `retry_after` base multiplied by
/// 1.5 per prior retry, capped at 10 s, then floored at 1 s. The floor is the
/// one deliberate arithmetic divergence from the reference implementation: a
/// `retry_after: 0` server would otherwise trigger a zero-delay retry whose
/// byte-identical event can mint the same second-resolution event id as the
/// original, which relays and the server's ingestion dedup then swallow.
fn compute_backoff(retry_after_secs: u64, retries: u32) -> Duration {
    let base_ms = retry_after_secs as f64 * 1000.0;
    let capped_ms = (base_ms * 1.5f64.powi(retries as i32)).min(10_000.0);
    Duration::from_millis(capped_ms.max(1_000.0) as u64)
}

/// The raw gating error re-addressed to the ORIGINAL inner request id (the id
/// the consumer sent). This SDK's servers already answer with the inner id (a
/// no-op here); the reference servers answer with the rewritten event id, and
/// the correlation entry is truthful in both cases.
fn normalized_raw(err: &JsonRpcErrorResponse, original_id: &serde_json::Value) -> JsonRpcMessage {
    JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
        jsonrpc: err.jsonrpc.clone(),
        id: original_id.clone(),
        error: err.error.clone(),
    })
}

/// The capability named by a cached request, for decline-error context:
/// `params.name` (tools/prompts) or `params.uri` (resources).
fn capability_of(request: &JsonRpcRequest) -> Option<String> {
    let params = request.params.as_ref()?;
    params
        .get("name")
        .or_else(|| params.get("uri"))?
        .as_str()
        .map(String::from)
}

fn lock<'a, T>(mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The client-side payment engine installed by `with_client_payments`.
///
/// Held by the client transport as `Option<Arc<ClientPaymentsEngine>>` and
/// invoked from its fixed inbound/outbound hook points. Never constructed
/// directly by applications.
pub(crate) struct ClientPaymentsEngine {
    /// In-band handlers indexed by PMI (duplicates resolved last-wins at
    /// construction, with a warning per duplicate).
    handlers_by_pmi: HashMap<String, Arc<dyn PaymentHandler>>,
    /// Optional transparent-lifecycle policy gate.
    payment_policy: Option<Arc<PaymentPolicyFn>>,
    /// Optional explicit-gating callback.
    on_payment_required: Option<Arc<OnPaymentRequiredFn>>,
    /// Beat interval for the shared heartbeat scheduler.
    synthetic_progress_interval: Duration,
    /// Keep-alive duration when the offer carries no `ttl`.
    default_payment_ttl: Duration,
    /// Cap on `-32043` retries per request.
    max_pending_retries: u32,
    /// The consumer channel: beats and synthesized errors are pushed here.
    message_tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
    /// The transport's send capability, for byte-true retries through the
    /// full production path.
    send_parts: ClientSendParts,
    /// The transport's pending-request store (the touch loop's target).
    pending: ClientCorrelationStore,
    /// Touch-loop cadence: `min(synthetic_progress_interval, correlation
    /// retention timeout / 2)`, so the refresh always lands with real margin
    /// before the sweep's expiry boundary.
    touch_cadence: Duration,
    /// In-flight `pay_req` dedup. The engine's OWN synchronous mutex: the
    /// claim happens synchronously before the handler chain is spawned, and
    /// no `.await` ever runs under the guard.
    in_flight_pay_reqs: Mutex<HashSet<String>>,
    /// Raw outbound requests, retry-capable, keyed by [`cache_key`].
    raw_request_cache: Mutex<LruCache<String, CachedRequest>>,
    /// Live heartbeats plus the scheduler flag.
    heartbeats: Mutex<HeartbeatState>,
    /// Per-payment touch loops, keyed by request event id.
    touch_loops: Mutex<HashMap<String, CancellationToken>>,
    /// Engine disposal (fired by transport close).
    cancel: CancellationToken,
}

impl ClientPaymentsEngine {
    /// Build the engine from validated options plus the transport handles it
    /// hooks into. `correlation_timeout` is the transport's retention TTL
    /// (`config.timeout`), which bounds the touch cadence.
    pub(crate) fn new(
        options: &ClientPaymentsOptions,
        message_tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
        send_parts: ClientSendParts,
        pending: ClientCorrelationStore,
        correlation_timeout: Duration,
    ) -> Arc<Self> {
        let mut handlers_by_pmi: HashMap<String, Arc<dyn PaymentHandler>> = HashMap::new();
        for handler in &options.handlers {
            if handlers_by_pmi.contains_key(handler.pmi()) {
                tracing::warn!(
                    target: LOG_TARGET,
                    pmi = %handler.pmi(),
                    "duplicate PMI handler registered, last one wins"
                );
            }
            handlers_by_pmi.insert(handler.pmi().to_string(), Arc::clone(handler));
        }

        Arc::new(Self {
            handlers_by_pmi,
            payment_policy: options.payment_policy.clone(),
            on_payment_required: options.on_payment_required.clone(),
            synthetic_progress_interval: options.synthetic_progress_interval,
            default_payment_ttl: options.default_payment_ttl,
            max_pending_retries: options.max_pending_retries,
            message_tx,
            send_parts,
            pending,
            touch_cadence: options
                .synthetic_progress_interval
                .min(correlation_timeout / 2),
            in_flight_pay_reqs: Mutex::new(HashSet::new()),
            raw_request_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(RAW_REQUEST_CACHE_CAPACITY).expect("capacity is non-zero"),
            )),
            heartbeats: Mutex::new(HeartbeatState::default()),
            touch_loops: Mutex::new(HashMap::new()),
            cancel: CancellationToken::new(),
        })
    }

    // ── Outbound hook ────────────────────────────────────────────────

    /// Record a raw outbound request for explicit-gating retries. Keyed by the
    /// request's own inner id; a re-send of the same id preserves the entry's
    /// retry counter.
    pub(crate) fn cache_raw_request(&self, message: &JsonRpcMessage) {
        if let JsonRpcMessage::Request(req) = message {
            let key = cache_key(&req.id);
            let mut cache = lock(&self.raw_request_cache);
            let pending_retries = cache.peek(&key).map(|c| c.pending_retries).unwrap_or(0);
            cache.put(
                key,
                CachedRequest {
                    request: req.clone(),
                    pending_retries,
                },
            );
        }
    }

    // ── Inbound notification hook ────────────────────────────────────

    /// React to a correlated payment notification. Returns whether the caller
    /// should still forward the notification to the consumer channel; any
    /// synthesized message (the immediate heartbeat, a decline, a rejection
    /// error) is pushed by the engine BEFORE this returns, so the consumer
    /// observes it first (the reference implementation's ordering).
    pub(crate) fn handle_payment_notification(
        self: &Arc<Self>,
        notif: &JsonRpcNotification,
        correlated_event_id: &str,
        entry: Option<PendingRequest>,
        requested_gating: bool,
        effective_gating: bool,
    ) -> bool {
        match notif.method.as_str() {
            PAYMENT_REQUIRED_METHOD => {
                self.on_payment_required_notification(
                    notif,
                    correlated_event_id,
                    entry,
                    requested_gating,
                    effective_gating,
                );
                true
            }
            PAYMENT_ACCEPTED_METHOD => {
                // Settlement acknowledged: the real response is imminent, so
                // the fabricated keep-alives stop here.
                self.stop_keepalives(correlated_event_id, entry.as_ref());
                true
            }
            PAYMENT_REJECTED_METHOD => {
                self.stop_keepalives(correlated_event_id, entry.as_ref());
                match entry {
                    Some(entry) => {
                        // The server never answers a rejected payment, so a
                        // synthesized error fails the caller's pending request
                        // immediately instead of letting it time out; the
                        // rejection notification itself is NOT forwarded.
                        let message = notif
                            .params
                            .as_ref()
                            .and_then(|p| {
                                serde_json::from_value::<PaymentRejectedParams>(p.clone()).ok()
                            })
                            .and_then(|p| p.message);
                        self.synthesize_error(
                            entry.original_id.clone(),
                            -32000,
                            match message {
                                Some(msg) => format!("Payment rejected: {msg}"),
                                None => "Payment rejected".to_string(),
                            },
                            None,
                        );
                        false
                    }
                    None => true,
                }
            }
            _ => true,
        }
    }

    /// The transparent pipeline on a correlated `payment_required`.
    fn on_payment_required_notification(
        self: &Arc<Self>,
        notif: &JsonRpcNotification,
        correlated_event_id: &str,
        entry: Option<PendingRequest>,
        requested_gating: bool,
        effective_gating: bool,
    ) {
        // Fail-closed serde is the validator: a malformed offer (fractional
        // amount, negative ttl) engages nothing, and the caller forwards the
        // raw notification so nothing is hidden from the application.
        let params: PaymentRequiredParams = match notif
            .params
            .as_ref()
            .map(|p| serde_json::from_value(p.clone()))
        {
            Some(Ok(params)) => params,
            _ => {
                tracing::debug!(
                    target: LOG_TARGET,
                    correlated_event_id = %correlated_event_id,
                    "unparseable payment_required params; leaving the notification to the app"
                );
                return;
            }
        };

        // A client that required explicit gating must not auto-satisfy
        // transparent offers in a session where the server did not accept it
        // (including a server that disclosed nothing): decline, do not pay.
        // The decline is synthesized BEFORE the caller forwards the
        // notification, matching the reference consumer ordering.
        if requested_gating && !effective_gating {
            tracing::warn!(
                target: LOG_TARGET,
                correlated_event_id = %correlated_event_id,
                pmi = %params.pmi,
                "declining transparent payment_required: explicit_gating was not accepted by the server"
            );
            self.synthesize_decline(
                entry.as_ref(),
                "Payment declined: explicit_gating was not accepted by the server",
                &params.pmi,
                params.amount,
            );
            return;
        }

        let ttl = params
            .ttl
            .map(Duration::from_secs)
            .unwrap_or(self.default_payment_ttl);

        // Keep-alives run BEFORE handler resolution so out-of-band and
        // unknown-PMI payments keep the request alive too. The heartbeat needs
        // the original progress token; the touch loop is token-independent and
        // runs for every payment with a positive TTL.
        if ttl > Duration::ZERO {
            if let Some(token) = entry.as_ref().and_then(|e| e.progress_token.as_ref()) {
                self.register_heartbeat(token, ttl);
            }
            self.start_touch_loop(correlated_event_id, ttl);
        }

        let Some(handler) = self.handlers_by_pmi.get(&params.pmi).cloned() else {
            tracing::debug!(
                target: LOG_TARGET,
                pmi = %params.pmi,
                correlated_event_id = %correlated_event_id,
                "no in-band handler for PMI; leaving payment to the application"
            );
            return;
        };

        // The dedup claim is synchronous, before the spawn, under the engine's
        // own lock: two deliveries of one offer can never both reach a wallet.
        {
            let mut in_flight = lock(&self.in_flight_pay_reqs);
            if !in_flight.insert(params.pay_req.clone()) {
                tracing::debug!(
                    target: LOG_TARGET,
                    correlated_event_id = %correlated_event_id,
                    "duplicate payment request already in flight, skipping"
                );
                return;
            }
        }

        tracing::info!(
            target: LOG_TARGET,
            correlated_event_id = %correlated_event_id,
            pmi = %params.pmi,
            amount = params.amount,
            "processing payment_required"
        );

        // The policy and wallet chain awaits arbitrary user code, so it runs
        // detached; the claim is released on EVERY exit, panics included.
        let engine = Arc::clone(self);
        let event_id = correlated_event_id.to_string();
        tokio::spawn(async move {
            let pay_req = params.pay_req.clone();
            if AssertUnwindSafe(engine.run_transparent_chain(handler, params, entry, event_id))
                .catch_unwind()
                .await
                .is_err()
            {
                tracing::warn!(
                    target: LOG_TARGET,
                    "payment handler chain panicked"
                );
            }
            lock(&engine.in_flight_pay_reqs).remove(&pay_req);
        });
    }

    /// The spawned policy / `can_handle` / `handle` chain for one offer.
    async fn run_transparent_chain(
        &self,
        handler: Arc<dyn PaymentHandler>,
        params: PaymentRequiredParams,
        entry: Option<PendingRequest>,
        correlated_event_id: String,
    ) {
        let request = PaymentHandlerRequest {
            amount: params.amount,
            pay_req: params.pay_req.clone(),
            pmi: params.pmi.clone(),
            description: params.description.clone(),
            ttl: params.ttl,
            meta: params.meta.clone(),
            request_event_id: correlated_event_id.clone(),
        };

        if let Some(policy) = &self.payment_policy {
            if !policy(request.clone()).await {
                tracing::debug!(
                    target: LOG_TARGET,
                    correlated_event_id = %correlated_event_id,
                    pmi = %request.pmi,
                    amount = request.amount,
                    "payment policy declined the payment"
                );
                self.stop_keepalives(&correlated_event_id, entry.as_ref());
                self.synthesize_decline(
                    entry.as_ref(),
                    "Payment declined by client policy",
                    &request.pmi,
                    request.amount,
                );
                return;
            }
        }

        if !handler.can_handle(&request).await {
            tracing::debug!(
                target: LOG_TARGET,
                correlated_event_id = %correlated_event_id,
                pmi = %request.pmi,
                "handler declined to handle"
            );
            self.stop_keepalives(&correlated_event_id, entry.as_ref());
            self.synthesize_decline(
                entry.as_ref(),
                "Payment declined by client handler",
                &request.pmi,
                request.amount,
            );
            return;
        }

        let pmi = request.pmi.clone();
        if let Err(error) = handler.handle(request).await {
            // Nothing is synthesized: the request stays pending, the
            // keep-alives keep running, and the server's TTL decides.
            tracing::warn!(
                target: LOG_TARGET,
                correlated_event_id = %correlated_event_id,
                pmi = %pmi,
                error = %error,
                "payment handler failed"
            );
        } else {
            tracing::info!(
                target: LOG_TARGET,
                correlated_event_id = %correlated_event_id,
                pmi = %pmi,
                "payment handler completed"
            );
        }
    }

    // ── Terminal hook (both push sites) ──────────────────────────────

    /// Terminal hook, shared by the single-event parse site and the oversized
    /// reassembly delivery: the pending entry for `message` has just been
    /// consumed. Stops the request's keep-alives, intercepts classified
    /// explicit-gating errors, clears retry state on a non-payment outcome,
    /// and returns the message to deliver to the consumer channel (`None`
    /// when the engine intercepted it and answers with a pay-or-retry flow).
    pub(crate) fn on_terminal_response(
        self: &Arc<Self>,
        entry: Option<PendingRequest>,
        correlated_event_id: Option<&str>,
        message: JsonRpcMessage,
    ) -> Option<JsonRpcMessage> {
        // The consumed entry answers this attempt: its keep-alives stop no
        // matter what kind of terminal this is (a gating retry re-registers
        // its own fresh entry, and a fresh offer re-registers the heartbeat).
        if let Some(event_id) = correlated_event_id {
            self.stop_touch_loop(event_id);
        }
        if let Some(token) = entry.as_ref().and_then(|e| e.progress_token.as_ref()) {
            self.stop_heartbeat(token);
        }

        // Explicit-gating classification. Identity resolves through the
        // consumed correlation entry, never the arriving wire id; without an
        // entry there is no identity to retry under, so the message passes
        // through untouched (degenerate passthrough, nothing normalized).
        if let (JsonRpcMessage::ErrorResponse(err), Some(entry)) = (&message, &entry) {
            if err.error.code == PAYMENT_REQUIRED_ERROR_CODE {
                let data = err
                    .error
                    .data
                    .as_ref()
                    .and_then(|d| {
                        serde_json::from_value::<PaymentRequiredErrorData>(d.clone()).ok()
                    })
                    .filter(|d| !d.payment_options.is_empty());
                if let Some(data) = data {
                    return self.intercept_payment_required_error(entry, err, data);
                }
            } else if err.error.code == PAYMENT_PENDING_ERROR_CODE {
                let data = err
                    .error
                    .data
                    .as_ref()
                    .and_then(|d| serde_json::from_value::<PaymentPendingErrorData>(d.clone()).ok())
                    .filter(|d| d.retry_after.is_some());
                if let Some(data) = data {
                    return self.intercept_payment_pending_error(entry, err, data);
                }
            }
        }

        // A terminal non-payment response retires the raw-request cache entry
        // and its embedded retry counter.
        if let Some(ref entry) = entry {
            let key = cache_key(&entry.original_id);
            lock(&self.raw_request_cache).pop(&key);
        }
        Some(message)
    }

    /// The `-32042 Payment Required` interception: pay through the configured
    /// callback and retry the cached original request. The enumerated give-up
    /// paths (no callback, cache miss) surface the RAW error re-addressed to
    /// the original inner id; everything else is answered by the spawned flow.
    fn intercept_payment_required_error(
        self: &Arc<Self>,
        entry: &PendingRequest,
        err: &JsonRpcErrorResponse,
        data: PaymentRequiredErrorData,
    ) -> Option<JsonRpcMessage> {
        let original_id = entry.original_id.clone();
        let Some(callback) = self.on_payment_required.clone() else {
            // The transparent client's shape: no gating handler configured, so
            // the raw error is the application's to act on.
            return Some(normalized_raw(err, &original_id));
        };
        let cached_request = {
            let mut cache = lock(&self.raw_request_cache);
            cache
                .get(&cache_key(&original_id))
                .map(|c| c.request.clone())
        };
        let Some(cached_request) = cached_request else {
            tracing::warn!(
                target: LOG_TARGET,
                "missing raw original request, cannot retry explicit payment"
            );
            return Some(normalized_raw(err, &original_id));
        };

        tracing::info!(
            target: LOG_TARGET,
            options_count = data.payment_options.len(),
            "invoking the payment-required callback for explicit gating"
        );
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = callback(PaymentRequiredCallbackParams {
                options: data.payment_options,
                instructions: data.instructions,
                original_request: cached_request.clone(),
            })
            .await;
            match outcome {
                Ok(approval) if approval.paid => {
                    tracing::info!(
                        target: LOG_TARGET,
                        method = %cached_request.method,
                        "explicit payment satisfied, retrying the original request"
                    );
                    // The retry replays the CACHED request byte-for-byte (same
                    // inner id, same method and params including _meta) under a
                    // fresh outer event, through the full production send path.
                    if let Err(error) = engine
                        .send_parts
                        .send(&JsonRpcMessage::Request(cached_request))
                        .await
                    {
                        // No synthesized error here: it would add a
                        // consumer-visible outcome the documented give-up
                        // surfaces do not include, and the reference
                        // implementation's consumer sees the same silence (it
                        // routes this failure to an error hook with no
                        // analogue here). The consumer's own request timeout
                        // resolves the attempt.
                        tracing::error!(
                            target: LOG_TARGET,
                            error = %error,
                            "failed to retry the request after an explicit payment"
                        );
                    }
                }
                Ok(approval) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        "the payment-required callback declined to pay"
                    );
                    engine.synthesize_error(
                        original_id,
                        PAYMENT_REQUIRED_ERROR_CODE,
                        "Payment Required".to_string(),
                        Some(serde_json::json!({
                            "reason": approval
                                .reason
                                .unwrap_or_else(|| "user_cancelled".to_string()),
                        })),
                    );
                }
                Err(error) => {
                    tracing::error!(
                        target: LOG_TARGET,
                        error = %error,
                        "the payment-required callback failed"
                    );
                    engine.synthesize_error(
                        original_id,
                        PAYMENT_REQUIRED_ERROR_CODE,
                        "Payment Required".to_string(),
                        Some(serde_json::json!({
                            "reason": error.to_string(),
                            "type": "payment_handler_error",
                        })),
                    );
                }
            }
        });
        None
    }

    /// The `-32043 Payment Pending` interception: retry the cached original
    /// request after a capped exponential backoff seeded by the server's
    /// `retry_after`. Cache misses and an exhausted retry budget surface the
    /// RAW error re-addressed to the original inner id.
    fn intercept_payment_pending_error(
        self: &Arc<Self>,
        entry: &PendingRequest,
        err: &JsonRpcErrorResponse,
        data: PaymentPendingErrorData,
    ) -> Option<JsonRpcMessage> {
        let original_id = entry.original_id.clone();
        let retry_after = data
            .retry_after
            .expect("classification requires retry_after");

        // The retry counter lives INSIDE the cache entry, so the cache's LRU
        // eviction bounds request and counter together.
        let (cached_request, retries) = {
            let mut cache = lock(&self.raw_request_cache);
            match cache.get_mut(&cache_key(&original_id)) {
                Some(cached) => {
                    let retries = cached.pending_retries;
                    if retries >= self.max_pending_retries {
                        (None, retries)
                    } else {
                        cached.pending_retries = retries + 1;
                        (Some(cached.request.clone()), retries)
                    }
                }
                None => {
                    drop(cache);
                    tracing::warn!(
                        target: LOG_TARGET,
                        "missing raw original request, cannot retry a pending payment"
                    );
                    return Some(normalized_raw(err, &original_id));
                }
            }
        };
        let Some(cached_request) = cached_request else {
            // Retries exhausted: the give-up surface is the server's own raw
            // error, nothing synthesized.
            tracing::error!(
                target: LOG_TARGET,
                max_retries = self.max_pending_retries,
                "maximum pending-payment retries exceeded"
            );
            return Some(normalized_raw(err, &original_id));
        };

        let delay = compute_backoff(retry_after, retries);
        tracing::info!(
            target: LOG_TARGET,
            retry_after_secs = retry_after,
            retry = retries + 1,
            delay_ms = delay.as_millis() as u64,
            "payment pending, retrying after backoff"
        );
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            tokio::select! {
                _ = engine.cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            if let Err(error) = engine
                .send_parts
                .send(&JsonRpcMessage::Request(cached_request))
                .await
            {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    "failed to retry the pending request"
                );
                engine.synthesize_error(
                    original_id,
                    PAYMENT_PENDING_ERROR_CODE,
                    "Failed to retry pending request".to_string(),
                    Some(serde_json::json!({ "reason": error.to_string() })),
                );
            }
        });
        None
    }

    // ── Heartbeats ──────────────────────────────────────────────────

    /// Register a synthetic-progress heartbeat for `token`, emit the immediate
    /// beat, and make sure the shared scheduler is running. No-op when the
    /// token is already tracked.
    fn register_heartbeat(self: &Arc<Self>, token: &serde_json::Value, ttl: Duration) {
        let key = token_key(token);
        {
            let mut state = lock(&self.heartbeats);
            if state.entries.contains_key(&key) {
                return;
            }
            state.entries.insert(
                key,
                HeartbeatEntry {
                    stop_at: Instant::now() + ttl,
                    token: token.clone(),
                },
            );
        }
        // Reset the MCP timeout immediately rather than waiting for the first
        // interval tick (a sub-interval request timeout would otherwise fire).
        self.emit_beat(token);
        self.ensure_scheduler();
    }

    /// Emit one synthetic `notifications/progress` beat carrying the ORIGINAL
    /// token JSON value (never a stringified form: rmcp's watcher map is keyed
    /// by exact JSON type).
    fn emit_beat(&self, token: &serde_json::Value) {
        let beat = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(serde_json::json!({
                "progressToken": token,
                "progress": 0,
            })),
        });
        let _ = self.message_tx.send(beat);
    }

    /// Start the shared beat scheduler when it is not already running and
    /// there is at least one live heartbeat.
    fn ensure_scheduler(self: &Arc<Self>) {
        {
            let mut state = lock(&self.heartbeats);
            if state.scheduler_running || state.entries.is_empty() {
                return;
            }
            state.scheduler_running = true;
        }
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(engine.synthetic_progress_interval);
            // An interval's first tick is immediate; the immediate beat was
            // already emitted at registration, so consume it.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = engine.cancel.cancelled() => {
                        lock(&engine.heartbeats).scheduler_running = false;
                        return;
                    }
                    _ = ticker.tick() => {}
                }
                let tokens: Vec<serde_json::Value> = {
                    let mut state = lock(&engine.heartbeats);
                    let now = Instant::now();
                    state.entries.retain(|_, entry| now < entry.stop_at);
                    if state.entries.is_empty() {
                        // The stop decision and the flag reset share the lock,
                        // so a concurrent registration either sees the running
                        // scheduler or restarts it; beats can never silently
                        // stop while entries exist.
                        state.scheduler_running = false;
                        return;
                    }
                    state.entries.values().map(|e| e.token.clone()).collect()
                };
                for token in tokens {
                    engine.emit_beat(&token);
                }
            }
        });
    }

    /// Stop the heartbeat for `token`, if one is live.
    fn stop_heartbeat(&self, token: &serde_json::Value) {
        lock(&self.heartbeats).entries.remove(&token_key(token));
    }

    // ── The payment-lifetime touch loop ─────────────────────────────

    /// Start the per-payment touch loop for `event_id`: refresh the pending
    /// entry on the bounded cadence for the payment's lifetime, so the
    /// transport's retention sweep cannot evict a request whose payment is
    /// still settling. Token-independent: it runs for token-less requests too.
    fn start_touch_loop(self: &Arc<Self>, event_id: &str, ttl: Duration) {
        let token = {
            let mut loops = lock(&self.touch_loops);
            if loops.contains_key(event_id) {
                return;
            }
            let token = self.cancel.child_token();
            loops.insert(event_id.to_string(), token.clone());
            token
        };
        let engine = Arc::clone(self);
        let event_id = event_id.to_string();
        let stop_at = Instant::now() + ttl;
        let cadence = self.touch_cadence;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(cadence) => {}
                }
                if Instant::now() >= stop_at {
                    break;
                }
                if !engine.pending.touch(&event_id).await {
                    // The entry is gone (consumed or swept): nothing to keep
                    // alive.
                    break;
                }
            }
            lock(&engine.touch_loops).remove(&event_id);
        });
    }

    /// Stop the touch loop for `event_id`, if one is running.
    fn stop_touch_loop(&self, event_id: &str) {
        if let Some(token) = lock(&self.touch_loops).remove(event_id) {
            token.cancel();
        }
    }

    /// Stop both keep-alives for one request.
    fn stop_keepalives(&self, event_id: &str, entry: Option<&PendingRequest>) {
        self.stop_touch_loop(event_id);
        if let Some(token) = entry.and_then(|e| e.progress_token.as_ref()) {
            self.stop_heartbeat(token);
        }
    }

    // ── Synthesis toward the consumer ───────────────────────────────

    /// Synthesize a `-32000` decline for the original request. No-op without a
    /// correlated pending entry to fail. The decline data carries the offer's
    /// PMI and amount, plus the original request's method and capability when
    /// the raw-request cache still holds it.
    fn synthesize_decline(
        &self,
        entry: Option<&PendingRequest>,
        message: &str,
        pmi: &str,
        amount: i64,
    ) {
        let Some(entry) = entry else {
            return;
        };
        let mut data = serde_json::Map::new();
        data.insert("pmi".to_string(), serde_json::json!(pmi));
        data.insert("amount".to_string(), serde_json::json!(amount));
        {
            let cache = lock(&self.raw_request_cache);
            if let Some(cached) = cache.peek(&cache_key(&entry.original_id)) {
                data.insert(
                    "method".to_string(),
                    serde_json::json!(cached.request.method),
                );
                if let Some(capability) = capability_of(&cached.request) {
                    data.insert("capability".to_string(), serde_json::json!(capability));
                }
            }
        }
        self.synthesize_error(
            entry.original_id.clone(),
            -32000,
            message.to_string(),
            Some(serde_json::Value::Object(data)),
        );
    }

    /// Push one synthesized JSON-RPC error to the local consumer.
    fn synthesize_error(
        &self,
        id: serde_json::Value,
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    ) {
        let error = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id,
            error: JsonRpcError {
                code,
                message,
                data,
            },
        });
        let _ = self.message_tx.send(error);
    }

    // ── Disposal ────────────────────────────────────────────────────

    /// Dispose all engine state on transport close: heartbeats, touch loops,
    /// the raw-request cache (and its embedded retry counters), and the
    /// in-flight dedup set.
    pub(crate) fn dispose(&self) {
        self.cancel.cancel();
        {
            let mut state = lock(&self.heartbeats);
            state.entries.clear();
        }
        {
            let mut loops = lock(&self.touch_loops);
            for (_, token) in loops.drain() {
                token.cancel();
            }
        }
        lock(&self.raw_request_cache).clear();
        lock(&self.in_flight_pay_reqs).clear();
    }
}

impl std::fmt::Debug for ClientPaymentsEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientPaymentsEngine")
            .field("handlers", &self.handlers_by_pmi.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use tracing_subscriber::fmt::MakeWriter;

    use crate::relay::mock::MockRelayPool;
    use crate::relay::RelayPoolTrait;
    use crate::transport::client::{NostrClientTransport, NostrClientTransportConfig};
    use nostr_sdk::prelude::Keys;

    struct EngineFx {
        engine: Arc<ClientPaymentsEngine>,
        rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
        pending: ClientCorrelationStore,
        pool: Arc<MockRelayPool>,
    }

    /// An engine over a mock transport's send parts, with its own consumer
    /// channel and pending store, driven directly (the bilateral e2e suite
    /// drives it through the production entry point instead). Encryption is
    /// disabled so retried requests are readable off the mock pool.
    async fn engine_fx(options: ClientPaymentsOptions, correlation_timeout: Duration) -> EngineFx {
        let pool = Arc::new(MockRelayPool::new());
        let transport = NostrClientTransport::with_relay_pool(
            NostrClientTransportConfig::default()
                .with_relay_urls(vec!["wss://mock.relay".to_string()])
                .with_server_pubkey(Keys::generate().public_key().to_hex())
                .with_encryption_mode(crate::core::types::EncryptionMode::Disabled),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("mock transport");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let pending = transport.correlation_store();
        let engine = ClientPaymentsEngine::new(
            &options,
            tx,
            transport.send_parts(),
            pending.clone(),
            correlation_timeout,
        );
        EngineFx {
            engine,
            rx,
            pending,
            pool,
        }
    }

    fn entry_with_token(token: Option<serde_json::Value>) -> PendingRequest {
        PendingRequest {
            original_id: serde_json::json!("req-1"),
            is_initialize: false,
            registered_at: Instant::now(),
            progress_token: token,
        }
    }

    fn required_notif(
        amount: i64,
        pay_req: &str,
        pmi: &str,
        ttl: Option<u64>,
    ) -> JsonRpcNotification {
        let mut params = serde_json::json!({
            "amount": amount,
            "pay_req": pay_req,
            "pmi": pmi,
        });
        if let Some(ttl) = ttl {
            params["ttl"] = serde_json::json!(ttl);
        }
        JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: PAYMENT_REQUIRED_METHOD.to_string(),
            params: Some(params),
        }
    }

    fn notif(method: &str, params: serde_json::Value) -> JsonRpcNotification {
        JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: Some(params),
        }
    }

    /// A handler that records the gate order and can decline or fail.
    struct RecordingHandler {
        pmi: String,
        calls: Arc<StdMutex<Vec<&'static str>>>,
        can_handle_result: bool,
        handle_error: Option<String>,
        delay: Duration,
    }

    impl RecordingHandler {
        fn new(calls: Arc<StdMutex<Vec<&'static str>>>) -> Self {
            Self {
                pmi: "fake".to_string(),
                calls,
                can_handle_result: true,
                handle_error: None,
                delay: Duration::ZERO,
            }
        }
    }

    #[async_trait]
    impl PaymentHandler for RecordingHandler {
        fn pmi(&self) -> &str {
            &self.pmi
        }
        async fn can_handle(&self, _req: &PaymentHandlerRequest) -> bool {
            self.calls.lock().unwrap().push("can_handle");
            self.can_handle_result
        }
        async fn handle(&self, _req: PaymentHandlerRequest) -> Result<(), PaymentError> {
            self.calls.lock().unwrap().push("handle");
            if self.delay > Duration::ZERO {
                tokio::time::sleep(self.delay).await;
            }
            match &self.handle_error {
                Some(msg) => Err(PaymentError::Handler(msg.clone())),
                None => Ok(()),
            }
        }
    }

    fn recording_policy(
        calls: Arc<StdMutex<Vec<&'static str>>>,
        approve: bool,
    ) -> Arc<PaymentPolicyFn> {
        Arc::new(move |_req: PaymentHandlerRequest| {
            let calls = Arc::clone(&calls);
            async move {
                calls.lock().unwrap().push("policy");
                approve
            }
            .boxed()
        })
    }

    async fn wait_until(what: &str, deadline: Duration, mut cond: impl FnMut() -> bool) {
        let end = tokio::time::Instant::now() + deadline;
        while !cond() {
            assert!(
                tokio::time::Instant::now() < end,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn drive(
        fx: &EngineFx,
        n: &JsonRpcNotification,
        entry: Option<PendingRequest>,
        requested_gating: bool,
        effective_gating: bool,
    ) -> bool {
        fx.engine.handle_payment_notification(
            n,
            "event-1",
            entry,
            requested_gating,
            effective_gating,
        )
    }

    // ── gate order + decline shapes ──────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipeline_runs_policy_then_can_handle_then_handle() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let options = ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(RecordingHandler::new(Arc::clone(&calls)))])
            .with_payment_policy(recording_policy(Arc::clone(&calls), true));
        let fx = engine_fx(options, Duration::from_secs(30)).await;

        let forward = drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        assert!(forward, "the notification is always forwarded");
        wait_until("the chain to settle", Duration::from_secs(2), || {
            calls.lock().unwrap().len() == 3
        })
        .await;
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["policy", "can_handle", "handle"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_decline_synthesizes_and_stops_short() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let options = ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(RecordingHandler::new(Arc::clone(&calls)))])
            .with_payment_policy(recording_policy(Arc::clone(&calls), false));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;

        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(Some(serde_json::json!(7)))),
            false,
            false,
        );

        // The immediate heartbeat precedes everything else on the channel.
        let beat = fx.rx.recv().await.expect("beat");
        assert!(matches!(beat, JsonRpcMessage::Notification(_)));
        let decline = fx.rx.recv().await.expect("decline");
        match decline {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.id, serde_json::json!("req-1"));
                assert_eq!(e.error.code, -32000);
                assert_eq!(e.error.message, "Payment declined by client policy");
                assert_eq!(
                    e.error.data,
                    Some(serde_json::json!({ "pmi": "fake", "amount": 21 })),
                    "no cached request: method and capability are omitted"
                );
            }
            other => panic!("expected the decline error, got {other:?}"),
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["policy"],
            "a policy decline must stop the chain before can_handle"
        );
        // The decline stopped the heartbeat.
        assert!(lock(&fx.engine.heartbeats).entries.is_empty());
        // The dedup claim was released: the same offer handles again.
        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        wait_until("the second chain", Duration::from_secs(2), || {
            calls.lock().unwrap().len() >= 2
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn can_handle_decline_names_the_handler() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let mut handler = RecordingHandler::new(Arc::clone(&calls));
        handler.can_handle_result = false;
        let options = ClientPaymentsOptions::new().with_handlers(vec![Arc::new(handler)]);
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;

        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        let decline = fx.rx.recv().await.expect("decline");
        match decline {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.error.message, "Payment declined by client handler");
            }
            other => panic!("expected the decline error, got {other:?}"),
        }
        assert_eq!(*calls.lock().unwrap(), vec!["can_handle"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decline_data_recovers_method_and_capability_from_the_cache() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let options = ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(RecordingHandler::new(Arc::clone(&calls)))])
            .with_payment_policy(recording_policy(Arc::clone(&calls), false));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;

        // The outbound hook cached the original request under its inner id.
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("req-1"),
                method: "tools/call".to_string(),
                params: Some(serde_json::json!({ "name": "paid-tool" })),
            }));

        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        let decline = fx.rx.recv().await.expect("decline");
        match decline {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(
                    e.error.data,
                    Some(serde_json::json!({
                        "pmi": "fake",
                        "amount": 21,
                        "method": "tools/call",
                        "capability": "paid-tool",
                    }))
                );
            }
            other => panic!("expected the decline error, got {other:?}"),
        }
    }

    // ── pay_req dedup ────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedup_claims_synchronously_and_releases_on_settle() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let mut handler = RecordingHandler::new(Arc::clone(&calls));
        handler.delay = Duration::from_millis(80);
        let options = ClientPaymentsOptions::new().with_handlers(vec![Arc::new(handler)]);
        let fx = engine_fx(options, Duration::from_secs(30)).await;

        // Two back-to-back deliveries of one offer: the claim is synchronous,
        // so the second never reaches a wallet.
        let n = required_notif(21, "inv-1", "fake", None);
        drive(&fx, &n, Some(entry_with_token(None)), false, false);
        drive(&fx, &n, Some(entry_with_token(None)), false, false);
        wait_until("the first handle", Duration::from_secs(2), || {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "handle")
                .count()
                == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "handle")
                .count(),
            1,
            "one settled payment per in-flight pay_req"
        );

        // In-flight-only semantics: after settling, an identical re-offer is
        // handled again (post-settle re-offers are the policy's domain).
        drive(&fx, &n, Some(entry_with_token(None)), false, false);
        wait_until("the re-offer handle", Duration::from_secs(2), || {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "handle")
                .count()
                == 2
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedup_keys_on_the_pay_req_not_the_amount() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let mut handler = RecordingHandler::new(Arc::clone(&calls));
        handler.delay = Duration::from_millis(80);
        let options = ClientPaymentsOptions::new().with_handlers(vec![Arc::new(handler)]);
        let fx = engine_fx(options, Duration::from_secs(30)).await;

        // Two DISTINCT payment requests with the same amount and PMI must both
        // reach the wallet while in flight together.
        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        drive(
            &fx,
            &required_notif(21, "inv-2", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        wait_until("both handles", Duration::from_secs(2), || {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "handle")
                .count()
                == 2
        })
        .await;
    }

    // ── mode mismatch ────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mode_mismatch_declines_without_paying() {
        // CEP-8: a client that required explicit_gating SHOULD NOT auto-satisfy
        // transparent payment_required in a session where the server did not
        // accept it. An undisclosed effective mode counts as not accepted.
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let options = ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(RecordingHandler::new(Arc::clone(&calls)))]);
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;

        let forward = drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(Some(serde_json::json!(7)))),
            true,  // requested explicit gating
            false, // the server never accepted it
        );
        assert!(forward, "the notification is still forwarded to the app");

        // The decline precedes the (caller-forwarded) notification; no beat is
        // emitted because the heartbeat is never registered on this path.
        let first = fx.rx.recv().await.expect("synthesized decline");
        match first {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.error.code, -32000);
                assert_eq!(
                    e.error.message,
                    "Payment declined: explicit_gating was not accepted by the server"
                );
            }
            other => panic!("expected the decline, got {other:?}"),
        }
        assert!(lock(&fx.engine.heartbeats).entries.is_empty());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "the handler must never run on a mode mismatch"
        );
        assert!(fx.rx.try_recv().is_err(), "no beat on this path");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepted_gating_session_still_auto_pays_transparent_offers() {
        // When the server DID accept explicit gating, a transparent offer in
        // the same session is still auto-paid (policy-gated), mirroring the
        // reference implementation's guard shape.
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let options = ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(RecordingHandler::new(Arc::clone(&calls)))]);
        let fx = engine_fx(options, Duration::from_secs(30)).await;

        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            true, // requested explicit gating
            true, // and the server accepted it
        );
        wait_until("the handle", Duration::from_secs(2), || {
            calls.lock().unwrap().contains(&"handle")
        })
        .await;
    }

    // ── heartbeat registration + shape ───────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn heartbeat_registration_rules() {
        let options = ClientPaymentsOptions::new();
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;

        // No token: no beat.
        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        assert!(fx.rx.try_recv().is_err(), "no token means no heartbeat");

        // Zero TTL: no beat even with a token.
        drive(
            &fx,
            &required_notif(21, "inv-2", "fake", Some(0)),
            Some(entry_with_token(Some(serde_json::json!(7)))),
            false,
            false,
        );
        assert!(fx.rx.try_recv().is_err(), "a zero ttl means no heartbeat");

        // A tokened offer beats immediately and synchronously, with the
        // ORIGINAL JSON token value (a number stays a number).
        drive(
            &fx,
            &required_notif(21, "inv-3", "fake", None),
            Some(entry_with_token(Some(serde_json::json!(7)))),
            false,
            false,
        );
        let beat = fx.rx.try_recv().expect("the immediate beat is synchronous");
        match beat {
            JsonRpcMessage::Notification(n) => {
                assert_eq!(n.method, "notifications/progress");
                assert_eq!(
                    n.params,
                    Some(serde_json::json!({ "progressToken": 7, "progress": 0 })),
                    "the beat must carry the exact original JSON token value"
                );
            }
            other => panic!("expected the beat, got {other:?}"),
        }

        // Already tracked: a second offer for the same token adds nothing.
        drive(
            &fx,
            &required_notif(21, "inv-4", "fake", None),
            Some(entry_with_token(Some(serde_json::json!(7)))),
            false,
            false,
        );
        assert!(
            fx.rx.try_recv().is_err(),
            "an already-tracked token must not beat again immediately"
        );
        assert_eq!(lock(&fx.engine.heartbeats).entries.len(), 1);

        // A string-typed token stays a string on the beat.
        drive(
            &fx,
            &required_notif(21, "inv-5", "fake", None),
            Some(entry_with_token(Some(serde_json::json!("7")))),
            false,
            false,
        );
        let beat = fx.rx.try_recv().expect("string-token beat");
        match beat {
            JsonRpcMessage::Notification(n) => {
                assert_eq!(
                    n.params,
                    Some(serde_json::json!({ "progressToken": "7", "progress": 0 })),
                );
            }
            other => panic!("expected the beat, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn heartbeat_stops_on_every_condition() {
        // Real time, tiny intervals: the scheduler interval and TTL are scaled
        // down; entry ages ride std::time::Instant, so paused time is not used
        // anywhere in this file.
        let options = ClientPaymentsOptions::new()
            .with_synthetic_progress_interval(Duration::from_millis(20))
            .with_default_payment_ttl(Duration::from_secs(5));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;
        let token = serde_json::json!(1);
        let offer = required_notif(21, "inv-1", "fake", None);
        let entry = || entry_with_token(Some(token.clone()));

        // Stop on payment_accepted.
        drive(&fx, &offer, Some(entry()), false, false);
        assert_eq!(lock(&fx.engine.heartbeats).entries.len(), 1);
        drive(
            &fx,
            &notif(
                PAYMENT_ACCEPTED_METHOD,
                serde_json::json!({ "amount": 21, "pmi": "fake" }),
            ),
            Some(entry()),
            false,
            false,
        );
        assert!(lock(&fx.engine.heartbeats).entries.is_empty());

        // Stop on payment_rejected.
        drive(&fx, &offer, Some(entry()), false, false);
        drive(
            &fx,
            &notif(
                PAYMENT_REJECTED_METHOD,
                serde_json::json!({ "pmi": "fake" }),
            ),
            Some(entry()),
            false,
            false,
        );
        assert!(lock(&fx.engine.heartbeats).entries.is_empty());

        // Stop on a terminal response through the shared helper.
        drive(&fx, &offer, Some(entry()), false, false);
        let delivered = fx.engine.on_terminal_response(
            Some(entry()),
            Some("event-1"),
            JsonRpcMessage::Response(crate::core::types::JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("req-1"),
                result: serde_json::json!({}),
            }),
        );
        assert!(delivered.is_some());
        assert!(lock(&fx.engine.heartbeats).entries.is_empty());

        // Stop at TTL expiry (offer-supplied default TTL scaled tiny).
        let options = ClientPaymentsOptions::new()
            .with_synthetic_progress_interval(Duration::from_millis(20))
            .with_default_payment_ttl(Duration::from_millis(60));
        let fx2 = engine_fx(options, Duration::from_secs(30)).await;
        drive(&fx2, &offer, Some(entry()), false, false);
        wait_until(
            "ttl expiry to drain the map",
            Duration::from_secs(2),
            || lock(&fx2.engine.heartbeats).entries.is_empty(),
        )
        .await;
        wait_until("the scheduler to stop", Duration::from_secs(2), || {
            !lock(&fx2.engine.heartbeats).scheduler_running
        })
        .await;

        // Stop on dispose (transport close).
        drive(&fx, &offer, Some(entry()), false, false);
        fx.engine.dispose();
        assert!(lock(&fx.engine.heartbeats).entries.is_empty());
        wait_until(
            "the scheduler to observe disposal",
            Duration::from_secs(2),
            || !lock(&fx.engine.heartbeats).scheduler_running,
        )
        .await;
        // Drain and confirm silence afterward.
        while fx.rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            fx.rx.try_recv().is_err(),
            "no beats after every stop condition fired"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn heartbeat_keeps_beating_until_stopped() {
        let options = ClientPaymentsOptions::new()
            .with_synthetic_progress_interval(Duration::from_millis(15));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;
        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(Some(serde_json::json!(7)))),
            false,
            false,
        );
        // The immediate beat plus at least two interval beats.
        let mut beats = 0;
        let end = tokio::time::Instant::now() + Duration::from_secs(2);
        while beats < 3 && tokio::time::Instant::now() < end {
            match fx.rx.try_recv() {
                Ok(JsonRpcMessage::Notification(n)) => {
                    assert_eq!(n.method, "notifications/progress");
                    beats += 1;
                }
                Ok(other) => panic!("unexpected message {other:?}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        assert!(
            beats >= 3,
            "expected the immediate beat plus interval beats"
        );
    }

    // ── rejected synthesis ───────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejected_synthesis_replaces_the_notification() {
        let options = ClientPaymentsOptions::new();
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;

        // With a live pending entry: synthesized error, notification replaced.
        let forward = drive(
            &fx,
            &notif(
                PAYMENT_REJECTED_METHOD,
                serde_json::json!({ "pmi": "fake", "message": "expired" }),
            ),
            Some(entry_with_token(None)),
            false,
            false,
        );
        assert!(!forward, "the rejection notification is not forwarded");
        let error = fx.rx.recv().await.expect("synthesized rejection");
        match error {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.id, serde_json::json!("req-1"));
                assert_eq!(e.error.code, -32000);
                assert_eq!(e.error.message, "Payment rejected: expired");
                assert!(e.error.data.is_none());
            }
            other => panic!("expected the rejection error, got {other:?}"),
        }

        // Without a message: the bare text.
        let forward = drive(
            &fx,
            &notif(
                PAYMENT_REJECTED_METHOD,
                serde_json::json!({ "pmi": "fake" }),
            ),
            Some(entry_with_token(None)),
            false,
            false,
        );
        assert!(!forward);
        match fx.rx.recv().await.expect("synthesized rejection") {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.error.message, "Payment rejected");
            }
            other => panic!("expected the rejection error, got {other:?}"),
        }

        // Without a live pending entry: forwarded untouched, nothing invented.
        // Through the transport this shape is unreachable (the correlation gate
        // drops correlated messages with no live entry before the hook runs);
        // it is pinned here deliberately as the engine-level contract.
        let forward = drive(
            &fx,
            &notif(
                PAYMENT_REJECTED_METHOD,
                serde_json::json!({ "pmi": "fake" }),
            ),
            None,
            false,
            false,
        );
        assert!(forward);
        assert!(fx.rx.try_recv().is_err());
    }

    // ── handler error ────────────────────────────────────────────────

    // Current-thread flavor deliberately: the log capture's subscriber guard
    // is thread-local, so the spawned handler chain must run on this thread.
    #[tokio::test]
    async fn handler_error_leaves_the_request_pending() {
        #[derive(Clone, Default)]
        struct Capture(Arc<StdMutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Capture {
            type Writer = Capture;
            fn make_writer(&'a self) -> Capture {
                self.clone()
            }
        }
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let calls = Arc::new(StdMutex::new(Vec::new()));
        let mut handler = RecordingHandler::new(Arc::clone(&calls));
        handler.handle_error = Some("wallet offline".to_string());
        let options = ClientPaymentsOptions::new().with_handlers(vec![Arc::new(handler)]);
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;

        drive(
            &fx,
            &required_notif(21, "secret-invoice-blob", "fake", None),
            Some(entry_with_token(Some(serde_json::json!(7)))),
            false,
            false,
        );
        wait_until("the failing handle", Duration::from_secs(2), || {
            calls.lock().unwrap().contains(&"handle")
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The immediate beat arrived; nothing else was synthesized.
        let beat = fx.rx.try_recv().expect("the immediate beat");
        assert!(matches!(beat, JsonRpcMessage::Notification(_)));
        assert!(
            fx.rx.try_recv().is_err(),
            "a handler failure synthesizes nothing"
        );
        // The request stays pending: the heartbeat keeps running to TTL.
        assert_eq!(lock(&fx.engine.heartbeats).entries.len(), 1);
        // The dedup claim was released.
        drive(
            &fx,
            &required_notif(21, "secret-invoice-blob", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        wait_until("the retry handle", Duration::from_secs(2), || {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "handle")
                .count()
                == 2
        })
        .await;
        // The warning names the failure but never the payment request blob.
        let logs = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
        assert!(
            logs.contains("payment handler failed"),
            "the failure must be logged, logs:\n{logs}"
        );
        assert!(
            !logs.contains("secret-invoice-blob"),
            "the payment request must never be logged, logs:\n{logs}"
        );
    }

    // ── the payment-lifetime touch loop ──────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn payment_touch_loop_outruns_the_sweep() {
        // Real time: a tiny correlation timeout (200 ms) puts the touch
        // cadence at its timeout/2 bound (100 ms); the offer's keep-alive
        // window (default TTL, 2 s here) spans several sweep windows.
        let options = ClientPaymentsOptions::new().with_default_payment_ttl(Duration::from_secs(2));
        let fx = engine_fx(options, Duration::from_millis(200)).await;
        let timeout = Duration::from_millis(200);

        fx.pending
            .register("event-1".into(), serde_json::json!("req-1"), false, None)
            .await;
        // Token-independent: the entry carries NO progress token, and the
        // loop must keep it alive regardless.
        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );

        // Several sweep windows pass; the touched entry survives every one.
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            fx.pending.sweep_expired(timeout).await;
            assert!(
                fx.pending.contains("event-1").await,
                "the touch loop must keep the paying request alive"
            );
        }

        // A stop condition fires (settlement acknowledged): the loop stops and
        // the sweep reclaims the entry once it ages out.
        drive(
            &fx,
            &notif(
                PAYMENT_ACCEPTED_METHOD,
                serde_json::json!({ "amount": 21, "pmi": "fake" }),
            ),
            Some(entry_with_token(None)),
            false,
            false,
        );
        wait_until("the loop to stop", Duration::from_secs(2), || {
            lock(&fx.engine.touch_loops).is_empty()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        fx.pending.sweep_expired(timeout).await;
        assert!(
            !fx.pending.contains("event-1").await,
            "after the stop condition the entry ages out normally"
        );
    }

    // ── options surface ──────────────────────────────────────────────

    #[test]
    fn options_defaults_match_the_shipped_constants() {
        let options = ClientPaymentsOptions::new();
        assert!(options.handlers.is_empty());
        assert_eq!(
            options.synthetic_progress_interval,
            Duration::from_millis(DEFAULT_SYNTHETIC_PROGRESS_INTERVAL_MS)
        );
        assert_eq!(
            options.default_payment_ttl,
            Duration::from_millis(DEFAULT_PAYMENT_TTL_MS)
        );
        assert!(options.payment_policy.is_none());
        assert!(options.payment_interaction.is_none());
        assert_eq!(options.max_pending_retries, 10);
        assert!(options.on_payment_required.is_none());
    }

    #[test]
    fn options_debug_redacts_callbacks_and_lists_pmis() {
        let options = ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(RecordingHandler::new(Arc::new(
                StdMutex::new(Vec::new()),
            )))])
            .with_payment_policy(recording_policy(Arc::new(StdMutex::new(Vec::new())), true));
        let debug = format!("{options:?}");
        assert!(debug.contains("fake"), "handlers appear as PMIs: {debug}");
        assert!(
            !debug.contains("Fn("),
            "callbacks must be redacted: {debug}"
        );
    }

    // ── explicit-gating interception ─────────────────────────────────

    fn gating_error(
        code: i64,
        wire_id: serde_json::Value,
        data: serde_json::Value,
    ) -> JsonRpcErrorResponse {
        JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: wire_id,
            error: JsonRpcError {
                code,
                message: if code == PAYMENT_REQUIRED_ERROR_CODE {
                    "Payment Required".to_string()
                } else {
                    "Payment Pending".to_string()
                },
                data: Some(data),
            },
        }
    }

    fn required_error_data() -> serde_json::Value {
        serde_json::json!({
            "payment_options": [
                { "amount": 21, "pmi": "fake", "pay_req": "inv-1" }
            ]
        })
    }

    fn declining_callback(reason: Option<&str>) -> Arc<OnPaymentRequiredFn> {
        let reason = reason.map(String::from);
        Arc::new(move |_params: PaymentRequiredCallbackParams| {
            let reason = reason.clone();
            async move {
                Ok(PaymentApproval {
                    paid: false,
                    reason,
                })
            }
            .boxed()
        })
    }

    fn paying_callback() -> Arc<OnPaymentRequiredFn> {
        Arc::new(|_params: PaymentRequiredCallbackParams| {
            async move {
                Ok(PaymentApproval {
                    paid: true,
                    reason: None,
                })
            }
            .boxed()
        })
    }

    fn original_request() -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("req-1"),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "paid-tool",
                "_meta": { "progressToken": 7 },
            })),
        }
    }

    /// The wire id of a gating error never matters: identity resolves through
    /// the consumed correlation entry, so an error whose id is the rewritten
    /// EVENT id (the reference server flavor) still hits the cache, and every
    /// surfaced error carries the ORIGINAL inner id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gating_identity_resolves_via_the_pending_entry() {
        let options =
            ClientPaymentsOptions::new().with_on_payment_required(declining_callback(None));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));

        // The error arrives with an event-id-flavored wire id, NOT the inner id.
        let err = gating_error(
            PAYMENT_REQUIRED_ERROR_CODE,
            serde_json::json!("f00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafe"),
            required_error_data(),
        );
        let intercepted = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(err),
        );
        assert!(
            intercepted.is_none(),
            "a classified offer with a cache hit is intercepted"
        );
        // The callback declined, so the synthesized error surfaces, carrying
        // the ORIGINAL inner id, never the wire id.
        let surfaced = fx.rx.recv().await.expect("synthesized decline");
        match surfaced {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.id, serde_json::json!("req-1"));
                assert_eq!(e.error.code, PAYMENT_REQUIRED_ERROR_CODE);
            }
            other => panic!("expected the synthesized error, got {other:?}"),
        }

        // Without a callback the raw error surfaces, normalized to the inner id.
        let options = ClientPaymentsOptions::new();
        let fx2 = engine_fx(options, Duration::from_secs(30)).await;
        fx2.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));
        let err = gating_error(
            PAYMENT_REQUIRED_ERROR_CODE,
            serde_json::json!("f00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafe"),
            required_error_data(),
        );
        let surfaced = fx2
            .engine
            .on_terminal_response(
                Some(entry_with_token(None)),
                Some("event-1"),
                JsonRpcMessage::ErrorResponse(err),
            )
            .expect("no callback: the raw error surfaces");
        match surfaced {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(
                    e.id,
                    serde_json::json!("req-1"),
                    "the surfaced raw error carries the original inner id"
                );
            }
            other => panic!("expected the raw error, got {other:?}"),
        }
    }

    /// A `{paid: true}` outcome replays the CACHED request byte-for-byte:
    /// same inner id, same method and params including `_meta`, published as
    /// a fresh outer event through the full production send path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paid_retry_replays_the_cached_request_byte_true() {
        let options = ClientPaymentsOptions::new().with_on_payment_required(paying_callback());
        let fx = engine_fx(options, Duration::from_secs(30)).await;
        let original = original_request();
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original.clone()));

        let err = gating_error(
            PAYMENT_REQUIRED_ERROR_CODE,
            serde_json::json!("req-1"),
            required_error_data(),
        );
        let intercepted = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(err),
        );
        assert!(intercepted.is_none());

        let pool = Arc::clone(&fx.pool);
        wait_until("the retry publish", Duration::from_secs(2), move || {
            let pool = Arc::clone(&pool);
            futures::executor::block_on(async move { !pool.stored_events().await.is_empty() })
        })
        .await;
        let events = fx.pool.stored_events().await;
        assert_eq!(events.len(), 1, "exactly one retry event");
        let replayed: JsonRpcMessage =
            serde_json::from_str(&events[0].content).expect("plaintext request");
        match replayed {
            JsonRpcMessage::Request(req) => {
                assert_eq!(
                    serde_json::to_value(&req).unwrap(),
                    serde_json::to_value(&original).unwrap(),
                    "the retry must be the cached original, byte-true"
                );
            }
            other => panic!("expected the replayed request, got {other:?}"),
        }
        // The retry also re-registered a pending entry with the token captured.
        assert_eq!(fx.pending.count().await, 1);
    }

    /// The raw-request cache lifecycle: populated at send, retained across
    /// gating errors, cleared on a terminal non-payment response, and a miss
    /// (eviction) surfaces the raw error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_request_cache_lifecycle() {
        let options =
            ClientPaymentsOptions::new().with_on_payment_required(declining_callback(None));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;
        let key = cache_key(&serde_json::json!("req-1"));

        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));
        assert!(lock(&fx.engine.raw_request_cache).peek(&key).is_some());

        // Retained across a classified gating error (the retry needs it).
        let err = gating_error(
            PAYMENT_REQUIRED_ERROR_CODE,
            serde_json::json!("req-1"),
            required_error_data(),
        );
        let intercepted = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(err),
        );
        assert!(intercepted.is_none());
        assert!(
            lock(&fx.engine.raw_request_cache).peek(&key).is_some(),
            "gating errors must not retire the cache entry"
        );
        let _ = fx.rx.recv().await; // drain the synthesized decline

        // Cleared on a terminal non-payment response.
        let delivered = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::Response(crate::core::types::JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("req-1"),
                result: serde_json::json!({}),
            }),
        );
        assert!(delivered.is_some());
        assert!(
            lock(&fx.engine.raw_request_cache).peek(&key).is_none(),
            "a terminal non-payment response retires the entry"
        );

        // A miss (never cached, or LRU-evicted) surfaces the raw error even
        // with a callback configured.
        let err = gating_error(
            PAYMENT_REQUIRED_ERROR_CODE,
            serde_json::json!("req-1"),
            required_error_data(),
        );
        let surfaced = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(err),
        );
        assert!(surfaced.is_some(), "a cache miss surfaces the raw error");
        let err = gating_error(
            PAYMENT_PENDING_ERROR_CODE,
            serde_json::json!("req-1"),
            serde_json::json!({ "retry_after": 1 }),
        );
        let surfaced = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(err),
        );
        assert!(
            surfaced.is_some(),
            "a pending-error cache miss surfaces the raw error"
        );
    }

    /// Degenerate shapes are never classified and pass through untouched,
    /// wire id included.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn degenerate_gating_shapes_pass_through_untouched() {
        let options =
            ClientPaymentsOptions::new().with_on_payment_required(declining_callback(None));
        let fx = engine_fx(options, Duration::from_secs(30)).await;
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));

        // A -32042 without a non-empty payment_options array.
        let err = gating_error(
            PAYMENT_REQUIRED_ERROR_CODE,
            serde_json::json!("wire-id"),
            serde_json::json!({ "payment_options": [] }),
        );
        let passed = fx
            .engine
            .on_terminal_response(
                Some(entry_with_token(None)),
                Some("event-1"),
                JsonRpcMessage::ErrorResponse(err),
            )
            .expect("degenerate passthrough");
        assert_eq!(
            passed.id(),
            Some(&serde_json::json!("wire-id")),
            "degenerate shapes keep their wire id untouched"
        );

        // A -32043 without retry_after.
        let err = gating_error(
            PAYMENT_PENDING_ERROR_CODE,
            serde_json::json!("wire-id"),
            serde_json::json!({ "instructions": "wait" }),
        );
        let passed = fx
            .engine
            .on_terminal_response(
                Some(entry_with_token(None)),
                Some("event-1"),
                JsonRpcMessage::ErrorResponse(err),
            )
            .expect("degenerate passthrough");
        assert_eq!(passed.id(), Some(&serde_json::json!("wire-id")));
    }

    /// The backoff arithmetic, pinned without a clock: base from
    /// `retry_after`, factor 1.5, cap 10 s, floor 1 s.
    #[test]
    fn compute_backoff_arithmetic() {
        // The base is the server's retry_after.
        assert_eq!(compute_backoff(2, 0), Duration::from_secs(2));
        // Factor 1.5 per prior retry.
        assert_eq!(compute_backoff(2, 1), Duration::from_millis(3000));
        assert_eq!(compute_backoff(2, 2), Duration::from_millis(4500));
        // Capped at 10 s no matter the retry count.
        assert_eq!(compute_backoff(2, 20), Duration::from_secs(10));
        assert_eq!(compute_backoff(60, 0), Duration::from_secs(10));
        // Floored at 1 s: a retry_after of 0 must never schedule a zero-delay
        // retry (a byte-identical same-second retry mints the same event id,
        // which relays and the server's ingestion dedup swallow).
        assert_eq!(compute_backoff(0, 0), Duration::from_secs(1));
        assert_eq!(compute_backoff(0, 5), Duration::from_secs(1));
    }

    /// The retry budget counts RETRIES, checked before each increment: with a
    /// cap of 1, the first pending error retries and the second surfaces raw
    /// (two total sends of the request, the initial one aside).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_retry_cap_counts_retries_not_attempts() {
        let options = ClientPaymentsOptions::new().with_max_pending_retries(1);
        let fx = engine_fx(options, Duration::from_secs(30)).await;
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));

        let pending_err = || {
            gating_error(
                PAYMENT_PENDING_ERROR_CODE,
                serde_json::json!("req-1"),
                serde_json::json!({ "retry_after": 1 }),
            )
        };
        // First pending error: retried (intercepted).
        let first = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(pending_err()),
        );
        assert!(first.is_none(), "the first pending error schedules a retry");

        // Second pending error: the budget (1) is spent, the raw error
        // surfaces with the original id.
        let second = fx
            .engine
            .on_terminal_response(
                Some(entry_with_token(None)),
                Some("event-1"),
                JsonRpcMessage::ErrorResponse(pending_err()),
            )
            .expect("retries exhausted: the raw error surfaces");
        match second {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.error.code, PAYMENT_PENDING_ERROR_CODE);
                assert_eq!(e.id, serde_json::json!("req-1"));
            }
            other => panic!("expected the raw pending error, got {other:?}"),
        }

        // The scheduled retry lands on the wire after the >= 1 s floor.
        let pool = Arc::clone(&fx.pool);
        wait_until(
            "the backoff retry publish",
            Duration::from_secs(3),
            move || {
                let pool = Arc::clone(&pool);
                futures::executor::block_on(async move { !pool.stored_events().await.is_empty() })
            },
        )
        .await;
        assert_eq!(fx.pool.stored_events().await.len(), 1);
    }

    /// The callback outcome surfaces exactly: a refusal synthesizes `-32042`
    /// with the reason (default `"user_cancelled"`), a callback failure adds
    /// `type: "payment_handler_error"`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn callback_outcomes_surface_exactly() {
        // paid: false with no reason.
        let options =
            ClientPaymentsOptions::new().with_on_payment_required(declining_callback(None));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));
        let intercepted = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(gating_error(
                PAYMENT_REQUIRED_ERROR_CODE,
                serde_json::json!("req-1"),
                required_error_data(),
            )),
        );
        assert!(intercepted.is_none());
        match fx.rx.recv().await.expect("synthesized error") {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.id, serde_json::json!("req-1"));
                assert_eq!(e.error.code, PAYMENT_REQUIRED_ERROR_CODE);
                assert_eq!(e.error.message, "Payment Required");
                assert_eq!(
                    e.error.data,
                    Some(serde_json::json!({ "reason": "user_cancelled" }))
                );
            }
            other => panic!("expected the synthesized error, got {other:?}"),
        }

        // paid: false with a reason.
        let options = ClientPaymentsOptions::new()
            .with_on_payment_required(declining_callback(Some("no funds")));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));
        fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(gating_error(
                PAYMENT_REQUIRED_ERROR_CODE,
                serde_json::json!("req-1"),
                required_error_data(),
            )),
        );
        match fx.rx.recv().await.expect("synthesized error") {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(
                    e.error.data,
                    Some(serde_json::json!({ "reason": "no funds" }))
                );
            }
            other => panic!("expected the synthesized error, got {other:?}"),
        }

        // A failing callback.
        let failing: Arc<OnPaymentRequiredFn> =
            Arc::new(|_params: PaymentRequiredCallbackParams| {
                async move { Err(PaymentError::Handler("wallet exploded".to_string())) }.boxed()
            });
        let options = ClientPaymentsOptions::new().with_on_payment_required(failing);
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));
        fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(gating_error(
                PAYMENT_REQUIRED_ERROR_CODE,
                serde_json::json!("req-1"),
                required_error_data(),
            )),
        );
        match fx.rx.recv().await.expect("synthesized error") {
            JsonRpcMessage::ErrorResponse(e) => {
                assert_eq!(e.error.code, PAYMENT_REQUIRED_ERROR_CODE);
                assert_eq!(
                    e.error.data,
                    Some(serde_json::json!({
                        "reason": PaymentError::Handler("wallet exploded".to_string())
                            .to_string(),
                        "type": "payment_handler_error",
                    }))
                );
            }
            other => panic!("expected the synthesized error, got {other:?}"),
        }
    }

    // ── the registration entry point ─────────────────────────────────

    async fn transport_with_pool(
        config: NostrClientTransportConfig,
    ) -> (NostrClientTransport, Arc<MockRelayPool>) {
        let pool = Arc::new(MockRelayPool::new());
        let transport = NostrClientTransport::with_relay_pool(
            config
                .with_relay_urls(vec!["wss://mock.relay".to_string()])
                .with_server_pubkey(Keys::generate().public_key().to_hex())
                .with_encryption_mode(crate::core::types::EncryptionMode::Disabled),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("mock transport");
        (transport, pool)
    }

    fn fake_handler() -> Arc<dyn PaymentHandler> {
        Arc::new(RecordingHandler::new(Arc::new(StdMutex::new(Vec::new()))))
    }

    fn probe_request() -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({ "name": "t" })),
        })
    }

    async fn last_event_tags(pool: &Arc<MockRelayPool>) -> Vec<Vec<String>> {
        pool.stored_events()
            .await
            .last()
            .expect("an event was published")
            .tags
            .iter()
            .map(|t| t.clone().to_vec())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn started_closed_and_double_registration_guards() {
        // Started: refused verbatim, and nothing is mutated (the config-seeded
        // PMI still rides the next request; the handler's PMI never appears).
        let (mut transport, pool) = transport_with_pool(
            NostrClientTransportConfig::default().with_pmis(vec!["seed".to_string()]),
        )
        .await;
        transport.start().await.expect("start");
        let error = with_client_payments(
            &mut transport,
            ClientPaymentsOptions::new().with_handlers(vec![fake_handler()]),
        )
        .expect_err("a post-start registration must be refused");
        assert_eq!(
            error.to_string(),
            "with_client_payments must be called before start()"
        );
        transport.send(&probe_request()).await.expect("send");
        let tags = last_event_tags(&pool).await;
        assert!(
            tags.contains(&vec!["pmi".to_string(), "seed".to_string()]),
            "the refused call must not replace the seeded PMIs, tags: {tags:?}"
        );
        assert!(
            !tags.contains(&vec!["pmi".to_string(), "fake".to_string()]),
            "the refused call must not advertise the handler PMI, tags: {tags:?}"
        );

        // Closed: `close()` takes the event-loop handle, so started-ness alone
        // would silently accept a registration that mutates PMIs on a dead
        // transport; the dedicated guard refuses it loudly.
        transport.close().await.expect("close");
        let error = with_client_payments(&mut transport, ClientPaymentsOptions::new())
            .expect_err("a post-close registration must be refused");
        assert_eq!(
            error.to_string(),
            "with_client_payments cannot register on a closed transport"
        );

        // Already registered: refused verbatim; the first registration stands.
        let (mut transport, pool) =
            transport_with_pool(NostrClientTransportConfig::default()).await;
        with_client_payments(
            &mut transport,
            ClientPaymentsOptions::new().with_handlers(vec![fake_handler()]),
        )
        .expect("the first registration succeeds");
        let mut other = RecordingHandler::new(Arc::new(StdMutex::new(Vec::new())));
        other.pmi = "other".to_string();
        let error = with_client_payments(
            &mut transport,
            ClientPaymentsOptions::new().with_handlers(vec![Arc::new(other)]),
        )
        .expect_err("a second registration must be refused");
        assert_eq!(
            error.to_string(),
            "client payments are already registered on this transport; \
             with_client_payments registers once and owns the payment surface"
        );
        transport.send(&probe_request()).await.expect("send");
        let tags = last_event_tags(&pool).await;
        assert!(
            tags.contains(&vec!["pmi".to_string(), "fake".to_string()]),
            "the first registration's PMI advertisement stands, tags: {tags:?}"
        );
        assert!(
            !tags.contains(&vec!["pmi".to_string(), "other".to_string()]),
            "the refused second registration must not advertise, tags: {tags:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registration_replaces_pmis_and_applies_the_mode() {
        // The handlers' PMIs REPLACE the config-seeded list, and a requested
        // mode overrides the config seed.
        let (mut transport, pool) = transport_with_pool(
            NostrClientTransportConfig::default().with_pmis(vec!["seed".to_string()]),
        )
        .await;
        with_client_payments(
            &mut transport,
            ClientPaymentsOptions::new()
                .with_handlers(vec![fake_handler()])
                .with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        )
        .expect("registers");
        assert!(transport.client_payments_installed());
        transport.send(&probe_request()).await.expect("send");
        let tags = last_event_tags(&pool).await;
        assert!(
            tags.contains(&vec!["pmi".to_string(), "fake".to_string()]),
            "the handler PMI must be advertised, tags: {tags:?}"
        );
        assert!(
            !tags.contains(&vec!["pmi".to_string(), "seed".to_string()]),
            "the config-seeded PMI list is replaced, tags: {tags:?}"
        );
        assert!(
            tags.contains(&vec![
                "payment_interaction".to_string(),
                "explicit_gating".to_string()
            ]),
            "the requested mode rides the first request, tags: {tags:?}"
        );

        // With no requested mode, the config's seed is left alone; with no
        // handlers, the advertisement is emptied (the out-of-band shape).
        let (mut transport, pool) = transport_with_pool(
            NostrClientTransportConfig::default()
                .with_pmis(vec!["seed".to_string()])
                .with_payment_interaction(PaymentInteractionMode::Transparent),
        )
        .await;
        with_client_payments(&mut transport, ClientPaymentsOptions::new()).expect("registers");
        transport.send(&probe_request()).await.expect("send");
        let tags = last_event_tags(&pool).await;
        assert!(
            !tags
                .iter()
                .any(|t| t.first().map(String::as_str) == Some("pmi")),
            "no handlers means no PMI advertisement, tags: {tags:?}"
        );
        assert!(
            tags.contains(&vec![
                "payment_interaction".to_string(),
                "transparent".to_string()
            ]),
            "an unset options mode leaves the config seed alone, tags: {tags:?}"
        );
    }

    // ── reconciliation pins: panic release, TTL stop, entry-less terminal,
    //    mismatched PMI ─────────────────────────────────────────────────

    /// A wallet handler that panics on its first call and succeeds after.
    struct PanicOnceHandler {
        pmi: String,
        handles: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl PaymentHandler for PanicOnceHandler {
        fn pmi(&self) -> &str {
            &self.pmi
        }
        async fn handle(&self, _req: PaymentHandlerRequest) -> Result<(), PaymentError> {
            let n = self
                .handles
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                panic!("wallet crashed mid-payment");
            }
            Ok(())
        }
    }

    /// The in-flight claim is released on EVERY exit, panics included: after a
    /// wallet handler panics, a fresh identical offer still reaches the wallet
    /// instead of being silently skipped forever (the never-pay inverse of
    /// double-pay).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicking_handler_still_releases_the_dedup_claim() {
        let handles = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let options =
            ClientPaymentsOptions::new().with_handlers(vec![Arc::new(PanicOnceHandler {
                pmi: "fake".to_string(),
                handles: Arc::clone(&handles),
            })]);
        let fx = engine_fx(options, Duration::from_secs(30)).await;

        let offer = required_notif(21, "inv-panic", "fake", None);
        drive(&fx, &offer, Some(entry_with_token(None)), false, false);
        wait_until("the panicking handle", Duration::from_secs(2), || {
            handles.load(std::sync::atomic::Ordering::SeqCst) == 1
        })
        .await;
        // The spawned chain unwinds and must still run its release.
        wait_until("the claim release", Duration::from_secs(2), || {
            lock(&fx.engine.in_flight_pay_reqs).is_empty()
        })
        .await;

        // The identical re-offer must reach the wallet again.
        drive(&fx, &offer, Some(entry_with_token(None)), false, false);
        wait_until("the post-panic handle", Duration::from_secs(2), || {
            handles.load(std::sync::atomic::Ordering::SeqCst) == 2
        })
        .await;
    }

    /// The touch loop lets go at the offer's TTL bound: an abandoned payment
    /// (the server goes silent after the offer) stops being kept alive, its
    /// task exits, and the sweep reclaims the entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn touch_loop_stops_at_the_offer_ttl_bound() {
        // Real time: cadence = min(50 ms, 300 ms / 2) = 50 ms; the keep-alive
        // window (default TTL) is 150 ms; no terminal ever arrives.
        let options = ClientPaymentsOptions::new()
            .with_synthetic_progress_interval(Duration::from_millis(50))
            .with_default_payment_ttl(Duration::from_millis(150));
        let fx = engine_fx(options, Duration::from_millis(300)).await;
        let timeout = Duration::from_millis(300);

        fx.pending
            .register("event-1".into(), serde_json::json!("req-1"), false, None)
            .await;
        drive(
            &fx,
            &required_notif(21, "inv-1", "fake", None),
            Some(entry_with_token(None)),
            false,
            false,
        );

        // The loop must exit at the TTL bound on its own, with no stop
        // condition fired.
        wait_until(
            "the loop to exit at its TTL",
            Duration::from_secs(2),
            || lock(&fx.engine.touch_loops).is_empty(),
        )
        .await;

        // With nothing refreshing it, the abandoned entry ages out.
        tokio::time::sleep(Duration::from_millis(350)).await;
        fx.pending.sweep_expired(timeout).await;
        assert!(
            !fx.pending.contains("event-1").await,
            "an abandoned payment's entry must be reclaimable after the TTL"
        );
    }

    /// A terminal with NO consumed entry passes through untouched even when it
    /// is a well-formed gating error: without an entry there is no identity to
    /// retry under, and the wire id is not normalized. This shape IS producible
    /// through the transport: the reassembled-delivery path sits above the
    /// correlation gate, so a reassembled terminal can arrive entry-less.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn entry_less_terminal_passes_through_even_when_classified_shaped() {
        let options =
            ClientPaymentsOptions::new().with_on_payment_required(declining_callback(None));
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));

        let err = gating_error(
            PAYMENT_REQUIRED_ERROR_CODE,
            serde_json::json!("wire-id"),
            required_error_data(),
        );
        let passed = fx
            .engine
            .on_terminal_response(None, Some("event-x"), JsonRpcMessage::ErrorResponse(err))
            .expect("no entry means passthrough, never interception");
        assert_eq!(
            passed.id(),
            Some(&serde_json::json!("wire-id")),
            "an entry-less terminal keeps its wire id untouched"
        );
        assert!(
            fx.rx.try_recv().is_err(),
            "nothing is synthesized without an identity"
        );
    }

    /// A POPULATED handler map offered a foreign PMI pays nothing: the offer is
    /// left to the application exactly like the no-handler shape.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mismatched_pmi_with_a_populated_handler_map_is_left_to_the_app() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let options = ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(RecordingHandler::new(Arc::clone(&calls)))]);
        let mut fx = engine_fx(options, Duration::from_secs(30)).await;

        let forward = drive(
            &fx,
            &required_notif(21, "inv-1", "zap", None),
            Some(entry_with_token(None)),
            false,
            false,
        );
        assert!(
            forward,
            "the foreign-PMI offer is still forwarded to the app"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "a foreign PMI must never reach the configured wallet"
        );
        assert!(
            fx.rx.try_recv().is_err(),
            "nothing is synthesized for a foreign-PMI offer"
        );
    }

    /// The pending-retry counter survives a re-cache of the same request id
    /// (the outbound hook preserves it), so a consumer re-send mid-retry-cycle
    /// cannot reset the budget. Matches the reference semantics, where a fresh
    /// send overwrites the cached request but leaves its separate counter map
    /// untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_retry_counter_survives_a_re_cache_of_the_same_id() {
        let options = ClientPaymentsOptions::new().with_max_pending_retries(1);
        let fx = engine_fx(options, Duration::from_secs(30)).await;
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));

        let pending_err = || {
            gating_error(
                PAYMENT_PENDING_ERROR_CODE,
                serde_json::json!("req-1"),
                serde_json::json!({ "retry_after": 1 }),
            )
        };
        // The first pending error spends the single retry.
        let first = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(pending_err()),
        );
        assert!(
            first.is_none(),
            "the first pending error schedules the retry"
        );

        // The consumer re-sends the same id (the outbound hook re-caches it).
        fx.engine
            .cache_raw_request(&JsonRpcMessage::Request(original_request()));

        // The counter must have survived: the budget is spent, so the second
        // pending error surfaces raw instead of scheduling another retry.
        let second = fx.engine.on_terminal_response(
            Some(entry_with_token(None)),
            Some("event-1"),
            JsonRpcMessage::ErrorResponse(pending_err()),
        );
        assert!(
            second.is_some(),
            "a re-cache of the same id must not reset the retry budget"
        );
    }
}
