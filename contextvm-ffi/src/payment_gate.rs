//! CEP-8 payment-gate middleware for `contextvm-ffi`.
//!
//! This module is the self-contained Rust core of Phase 2.  It prices
//! `tools/call` invocations, parks them according to the configured lifecycle,
//! emits [`PaymentGateRequest`] events over a bounded mpsc queue, and answers
//! four foreign operations: `submit_invoice`, `mark_settled`, `mark_failed`,
//! and `mark_replayed`.
//!
//! Wire emissions are sent through an injected [`PaymentGateTransport`] so that
//! W1c can bridge the gate to the real `NostrServerTransport` without this
//! module depending on transport internals.

// Phase 3 wires this module into the UniFFI `Server` start() path.  A few
// helpers (e.g. parse_priced_capabilities_json) remain test-only.

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::core::types::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, PaymentInteractionMode,
};
use contextvm_sdk::payments::authorization_store::{AuthorizationStore, ClaimOrPending};
use contextvm_sdk::payments::canonical::{
    compute_canonical_invocation_identity, CanonicalInvocationIdentity,
};
use contextvm_sdk::payments::constants::{
    PAYMENT_ACCEPTED_METHOD, PAYMENT_REJECTED_METHOD, PAYMENT_REQUIRED_METHOD,
    PMI_BITCOIN_LIGHTNING_BOLT11,
};
use contextvm_sdk::payments::types::{
    Meta, PaymentAcceptedParams, PaymentOption, PaymentRejectedParams, PaymentRequiredParams,
};
use contextvm_sdk::payments::{build_payment_pending_error, build_payment_required_error};
use contextvm_sdk::transport::server::{InboundContext, InboundMiddleware, Next};

use crate::error::{ErrorCode, FfiError};

/// Extra headroom on top of the submitted invoice TTL + execution budget.
///
/// The gate enforces `ttl + execution_budget + margin < min(request_timeout, session_timeout)`
/// so a payment round-trip fits inside the route's overall timeout.
const ROUTE_BUDGET_MARGIN_SECS: u64 = 5;

/// Payment lifecycle modes implemented by the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentLifecyclePolicy {
    /// The paid request is held (parked) and forwarded after settlement.
    /// The gate emits `payment_required`, `payment_accepted` and
    /// `payment_rejected` lifecycle notifications through the transport.
    #[default]
    Transparent,
    /// The original request is answered with a targeted `-32042` and dropped.
    /// The client must retry; a single-use grant is consumed on the retry.
    /// No lifecycle notifications are sent.
    Gating,
}

/// A priced capability advertised by the server.
///
/// JSON in the wire plan uses camelCase (`maxAmount`, `currencyUnit`, `amount`).
/// The Rust struct keeps the naming requested by the FFI plan
/// (`amount_sats`, `max_amount_sats`, `currency_unit`) while parsing both forms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricedCapability {
    /// JSON-RPC method this capability prices.
    pub method: String,
    /// Capability name.  An empty string is treated as a wildcard match
    /// against any `params.name` in the request.
    #[serde(default)]
    pub name: String,
    /// Minimum / advertised price in sats.
    #[serde(rename = "amount")]
    pub amount_sats: i64,
    /// Optional maximum price in sats.  When present it must be `>= amount_sats`.
    #[serde(rename = "maxAmount")]
    pub max_amount_sats: Option<i64>,
    /// Currency unit.  The CEP-8 MVP only accepts `"sats"`.
    #[serde(rename = "currencyUnit")]
    pub currency_unit: String,
    /// Human-readable description of the capability.
    #[serde(default)]
    pub description: String,
}

impl PricedCapability {
    /// Validate unit and amount constraints.
    pub fn validate(&self) -> Result<(), FfiError> {
        if self.currency_unit != "sats" {
            return Err(validation_error(
                "PricedCapability.currency_unit must be 'sats'",
            ));
        }
        if self.amount_sats <= 0 {
            return Err(validation_error("PricedCapability.amount_sats must be > 0"));
        }
        if let Some(max) = self.max_amount_sats {
            if max < self.amount_sats {
                return Err(validation_error(
                    "PricedCapability.max_amount_sats must be >= amount_sats",
                ));
            }
        }
        Ok(())
    }
}

impl From<&contextvm_sdk::payments::types::PricedCapability> for PricedCapability {
    fn from(cap: &contextvm_sdk::payments::types::PricedCapability) -> Self {
        Self {
            method: cap.method.clone(),
            name: cap.name.clone().unwrap_or_default(),
            amount_sats: cap.amount,
            max_amount_sats: cap.max_amount,
            currency_unit: cap.currency_unit.clone(),
            description: cap.description.clone().unwrap_or_default(),
        }
    }
}

/// Configuration for the payment gate.
#[derive(Debug, Clone)]
pub struct PaymentGateConfig {
    /// Maximum invoice TTL the operator is willing to accept, in seconds.
    pub payment_ttl_cap_secs: u64,
    /// Budget reserved for executing a paid call once it is forwarded.
    pub execution_budget_secs: u64,
    /// Server route request timeout in seconds.
    pub request_timeout_secs: u64,
    /// Server session timeout in seconds.
    pub session_timeout_secs: u64,
    /// Maximum number of canonical identities that may be parked concurrently.
    pub parked_cap: usize,
    /// Bound of the outbound mpsc queue carrying [`PaymentGateRequest`] events.
    pub event_queue_bound: usize,
    /// Default lifecycle policy for the gate.
    pub policy: PaymentLifecyclePolicy,
    /// Capabilities that require payment.
    pub priced_capabilities: Vec<PricedCapability>,
}

impl Default for PaymentGateConfig {
    fn default() -> Self {
        Self {
            payment_ttl_cap_secs: 300,
            execution_budget_secs: 600,
            request_timeout_secs: 900,
            session_timeout_secs: 1200,
            parked_cap: 128,
            event_queue_bound: 64,
            policy: PaymentLifecyclePolicy::Transparent,
            priced_capabilities: Vec::new(),
        }
    }
}

/// A request emitted to the foreign consumer (W1c) when a paid call is parked.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PaymentGateRequest {
    /// Original request Nostr event id.
    pub request_event_id: String,
    /// Client pubkey that sent the request.
    pub client_pubkey: String,
    /// JSON-RPC method being priced (currently always `tools/call`).
    pub method: String,
    /// JSON-stringified request parameters.
    pub params_json: String,
    /// Capability name matched from the request parameters.
    pub capability_name: String,
    /// Canonical invocation identity (`client_pubkey:invocation_hash`).
    pub canonical_invocation_id: String,
}

/// Transport seam used by the gate to send lifecycle notifications and targeted
/// JSON-RPC responses.
pub trait PaymentGateTransport: Send + Sync {
    /// Send a `payment_required`, `payment_accepted`, or `payment_rejected`
    /// notification.
    fn send_payment_notification(
        &self,
        client_pubkey: String,
        request_event_id: String,
        mirrored_wrap_kind: Option<u16>,
        notification: JsonRpcMessage,
    ) -> Pin<Box<dyn Future<Output = contextvm_sdk::Result<()>> + Send>>;

    /// Send a targeted JSON-RPC error response for a parked request.
    fn send_targeted_response(
        &self,
        client_pubkey: String,
        request_event_id: String,
        response: JsonRpcMessage,
    ) -> Pin<Box<dyn Future<Output = contextvm_sdk::Result<()>> + Send>>;
}

/// Internal abstraction for the `Next` middleware continuation.
///
/// `Next` is not constructible outside the SDK, so the gate wraps it behind a
/// small internal trait.  Unit tests supply a fake implementation that records
/// the forwarded message.
pub(crate) trait PaymentNext: Send {
    /// Keep the request's route alive without forwarding the message yet.
    fn keep_alive(&self);

    /// Stamp this continuation with a canonical invocation id.
    fn set_canonical_invocation_id(&self, id: String);

    /// Consume the continuation and forward `message` down the chain.
    fn run(self: Box<Self>, message: JsonRpcMessage) -> Pin<Box<dyn Future<Output = bool> + Send>>;

    /// Release a kept-alive request without forwarding it.
    fn release(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

struct SdkNext(Next);

impl PaymentNext for SdkNext {
    fn keep_alive(&self) {
        self.0.keep_alive();
    }

    fn set_canonical_invocation_id(&self, id: String) {
        self.0.set_canonical_invocation_id(id);
    }

    fn run(self: Box<Self>, message: JsonRpcMessage) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        let next = (*self).0;
        Box::pin(async move { next.run(message).await })
    }

    fn release(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let next = (*self).0;
        Box::pin(async move { next.release().await })
    }
}

/// Shared handle to the parked request continuation.
///
/// Publication is exclusive (a second `submit_invoice` while one is in flight
/// is rejected), so exactly one attempt can commit or roll back; this handle
/// lets rollback/failure paths release the `Next` exactly once when the entry
/// is removed.
type NextHandle = Arc<Mutex<Option<Box<dyn PaymentNext>>>>;

/// The payment gate middleware.
///
/// Clone is cheap: it bumps the reference count of the internal `Arc`.
#[derive(Clone)]
pub struct PaymentGate {
    inner: Arc<Inner>,
}

struct Inner {
    config: PaymentGateConfig,
    transport: Arc<dyn PaymentGateTransport>,
    events_tx: tokio::sync::mpsc::Sender<PaymentGateRequest>,
    events_rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<PaymentGateRequest>>>,
    parking: Mutex<Parking>,
    auth_store: AuthorizationStore,
    nonce: AtomicU64,
}

struct Parking {
    by_key: HashMap<String, ParkedEntry>,
    by_event: HashMap<String, String>,
    by_payreq: HashMap<String, String>,
    order: VecDeque<String>,
}

struct ParkedEntry {
    identity: CanonicalInvocationIdentity,
    request_event_id: String,
    client_pubkey: String,
    mirrored_wrap_kind: Option<u16>,
    capability: PricedCapability,
    request: JsonRpcRequest,
    next_and_message: Option<(NextHandle, JsonRpcMessage)>,
    state: PaymentState,
    expires_at: tokio::time::Instant,
    nonce: u64,
    lifecycle: PaymentLifecyclePolicy,
}

/// A cloneable snapshot of a [`ParkedEntry`] used by the async middleware
/// handler without holding a `parking_lot` guard across `.await` points.
#[derive(Debug, Clone)]
struct ParkedEntrySnapshot {
    identity: CanonicalInvocationIdentity,
    client_pubkey: String,
    request_event_id: String,
    mirrored_wrap_kind: Option<u16>,
    state: PaymentState,
    expires_at: tokio::time::Instant,
    capability: PricedCapability,
}

impl From<&ParkedEntry> for ParkedEntrySnapshot {
    fn from(entry: &ParkedEntry) -> Self {
        Self {
            identity: entry.identity.clone(),
            client_pubkey: entry.client_pubkey.clone(),
            request_event_id: entry.request_event_id.clone(),
            mirrored_wrap_kind: entry.mirrored_wrap_kind,
            state: entry.state.clone(),
            expires_at: entry.expires_at,
            capability: entry.capability.clone(),
        }
    }
}

#[derive(Debug, Clone)]
enum PaymentState {
    AwaitingInvoice,
    /// An invoice has been handed to the gate and the transport send is in
    /// flight. The `nonce` on the [`ParkedEntry`] identifies which attempt
    /// currently owns this state so a failed or superseded attempt cannot
    /// clobber a newer successful one.
    Publishing {
        pay_req: String,
        amount: i64,
        pmi: String,
        ttl_secs: u64,
        description: Option<String>,
    },
    InvoiceIssued {
        pay_req: String,
        amount: i64,
        pmi: String,
        ttl_secs: u64,
        description: Option<String>,
    },
    Granted,
    Claiming,
}

impl PaymentGate {
    /// Create a new payment gate with the supplied configuration and transport.
    ///
    /// Validates the configured capabilities (sats only, positive amount,
    /// `max >= min`) and clamps `parked_cap` and `event_queue_bound` to at
    /// least one.
    pub fn new(
        mut config: PaymentGateConfig,
        transport: Arc<dyn PaymentGateTransport>,
    ) -> Result<Self, FfiError> {
        for cap in &config.priced_capabilities {
            cap.validate()?;
        }
        if config.parked_cap == 0 {
            config.parked_cap = 1;
        }
        if config.event_queue_bound == 0 {
            config.event_queue_bound = 1;
        }

        let (events_tx, events_rx) = tokio::sync::mpsc::channel(config.event_queue_bound);

        Ok(Self {
            inner: Arc::new(Inner {
                config,
                transport,
                events_tx,
                events_rx: Arc::new(tokio::sync::Mutex::new(events_rx)),
                parking: Mutex::new(Parking {
                    by_key: HashMap::new(),
                    by_event: HashMap::new(),
                    by_payreq: HashMap::new(),
                    order: VecDeque::new(),
                }),
                auth_store: AuthorizationStore::new(),
                nonce: AtomicU64::new(1),
            }),
        })
    }

    /// Try to receive a [`PaymentGateRequest`] without waiting.
    #[cfg(test)]
    pub fn try_recv(&self) -> Option<PaymentGateRequest> {
        if let Ok(mut rx) = self.inner.events_rx.try_lock() {
            rx.try_recv().ok()
        } else {
            None
        }
    }

    /// Wait up to `timeout` for a [`PaymentGateRequest`].
    pub async fn recv_timeout(&self, timeout: Duration) -> Option<PaymentGateRequest> {
        tokio::time::timeout(timeout, async {
            let mut rx = self.inner.events_rx.lock().await;
            rx.recv().await
        })
        .await
        .ok()
        .flatten()
    }

    /// The foreign consumer has produced a payment invoice for the parked
    /// request identified by `request_event_id`.
    ///
    /// Validates amount, PMI allowlist, and TTL against the route budget, then
    /// transitions the gate state to `InvoiceIssued`.  In gating mode the
    /// original request is answered with a targeted `-32042`; in transparent
    /// mode a `payment_required` notification is emitted.
    pub async fn submit_invoice(
        &self,
        request_event_id: &str,
        amount_sats: i64,
        pay_req: &str,
        pmi: &str,
        ttl_secs: u64,
        description: Option<&str>,
    ) -> Result<(), FfiError> {
        let (canonical_key, nonce, expires_at, lifecycle, next_handle) = self.prepare_invoice(
            request_event_id,
            amount_sats,
            pay_req,
            pmi,
            ttl_secs,
            description,
        )?;

        let send_result = if lifecycle == PaymentLifecyclePolicy::Gating {
            self.send_payment_required_error(&canonical_key, nonce)
                .await
        } else {
            self.send_payment_required_notification(&canonical_key, nonce)
                .await
        };

        if let Err(e) = send_result {
            self.rollback_invoice(&canonical_key, lifecycle, next_handle, nonce)
                .await;
            return Err(e);
        }

        // Commit the in-flight `Publishing` state to `InvoiceIssued` only if
        // this nonce still owns it.
        if !self.finalize_invoice(&canonical_key, nonce) {
            // Another attempt won or the entry was removed. Release the request
            // if we still hold the only copy.
            if let Some(handle) = next_handle {
                let next = {
                    let mut guard = handle.lock();
                    guard.take()
                };
                if let Some(next) = next {
                    next.release().await;
                }
            }
            return Err(payment_error(
                "invoice was superseded before it could be finalized",
            ));
        }

        // Gating: the targeted response has been sent, so release the original
        // request. The shared handle ensures this happens exactly once even when
        // another attempt is racing the rollback.
        if let Some(handle) = next_handle {
            let next = {
                let mut guard = handle.lock();
                guard.take()
            };
            if let Some(next) = next {
                next.release().await;
            }
        }

        tokio::spawn(self.clone().ttl_worker(canonical_key, nonce, expires_at));
        Ok(())
    }

    /// The foreign consumer reports that the invoice identified by `pay_req`
    /// has been settled.
    ///
    /// In transparent mode a `payment_accepted` notification is sent strictly
    /// before the parked `Next` continuation is run.  In gating mode the gate
    /// stores a single-use grant and waits for the client to retry.
    pub async fn mark_settled(
        &self,
        pay_req: &str,
        meta_json: Option<&str>,
    ) -> Result<(), FfiError> {
        let meta = parse_meta_json(meta_json)?;
        let outcome = self.prepare_settle(pay_req).await?;

        match outcome {
            SettleOutcome::Transparent {
                identity,
                next_and_message,
                client_pubkey,
                request_event_id,
                mirrored_wrap_kind,
                amount,
                pmi,
                ttl_ms,
            } => {
                let Some((next, message)) = next_and_message else {
                    return Err(payment_error("no parked request to forward"));
                };

                // Grant and immediately consume it under one lock so the canonical id
                // is paid for the single forward we are about to perform and can never
                // be stolen by a concurrent duplicate.
                if !self.inner.auth_store.grant_and_claim(&identity, ttl_ms) {
                    return Err(payment_error(
                        "settlement grant was concurrently consumed or expired",
                    ));
                }

                let accepted = PaymentAcceptedParams { amount, pmi, meta };
                let _ = self
                    .inner
                    .transport
                    .send_payment_notification(
                        client_pubkey,
                        request_event_id,
                        mirrored_wrap_kind,
                        JsonRpcMessage::Notification(JsonRpcNotification {
                            jsonrpc: "2.0".into(),
                            method: PAYMENT_ACCEPTED_METHOD.into(),
                            params: Some(serde_json::to_value(accepted).expect("serialize")),
                        }),
                    )
                    .await;

                if next.run(message).await {
                    Ok(())
                } else {
                    Err(payment_error("failed to forward settled request"))
                }
            }
            SettleOutcome::Gating {
                canonical_key,
                nonce,
                expires_at,
            } => {
                tokio::spawn(self.clone().ttl_worker(canonical_key, nonce, expires_at));
                Ok(())
            }
        }
    }

    /// The foreign consumer reports that the invoice identified by `pay_req`
    /// has failed or cannot be verified.
    ///
    /// In transparent mode a `payment_rejected` notification is emitted; in
    /// gating mode the state is silently cleared.
    pub async fn mark_failed(&self, pay_req: &str, message: &str) -> Result<(), FfiError> {
        let canonical_key = {
            let parking = self.inner.parking.lock();
            parking
                .by_payreq
                .get(pay_req)
                .cloned()
                .ok_or_else(|| payment_error("unknown pay_req"))?
        };

        let Some(entry) = self.remove_and_clear(&canonical_key).await else {
            return Err(payment_error("parked entry disappeared"));
        };

        if entry.lifecycle == PaymentLifecyclePolicy::Transparent {
            let (pmi, amount) = payment_rejection_details(&entry);
            self.send_payment_rejected(
                &entry.client_pubkey,
                &entry.request_event_id,
                entry.mirrored_wrap_kind,
                pmi,
                Some(amount),
                Some(message),
            )
            .await;
        }

        Ok(())
    }

    /// The foreign consumer has a cached terminal result for the parked request
    /// identified by `request_event_id`.
    ///
    /// In both transparent and gating mode the current request's `Next` is
    /// forwarded free and the local entry is removed, so a replay never becomes a
    /// reusable free-authorization.
    pub async fn mark_replayed(&self, request_event_id: &str) -> Result<(), FfiError> {
        let next_and_message = self.prepare_replay(request_event_id).await?;

        if let Some((next, message)) = next_and_message {
            if next.run(message).await {
                Ok(())
            } else {
                Err(payment_error("failed to forward replayed request"))
            }
        } else {
            Err(payment_error("no parked request to replay"))
        }
    }

    /// Validate a JSON string containing a list of priced capabilities.
    #[cfg(test)]
    pub fn parse_priced_capabilities_json(s: &str) -> Result<Vec<PricedCapability>, FfiError> {
        let caps: Vec<PricedCapability> = serde_json::from_str(s).map_err(|e| FfiError {
            code: ErrorCode::Validation,
            message: format!("invalid priced capabilities JSON: {e}"),
        })?;
        for cap in &caps {
            cap.validate()?;
        }
        Ok(caps)
    }
}

// Internal helpers.
impl PaymentGate {
    /// Core middleware entry point, also used by unit tests with a fake `Next`.
    pub(crate) async fn handle_inner(
        &self,
        message: JsonRpcMessage,
        ctx: &InboundContext,
        next: Box<dyn PaymentNext>,
    ) -> bool {
        let request = match message {
            JsonRpcMessage::Request(r) => r,
            _ => return next.run(message).await,
        };

        if request.method != "tools/call" {
            return next.run(JsonRpcMessage::Request(request)).await;
        }

        let Some(capability) = self.match_priced_capability(&request) else {
            return next.run(JsonRpcMessage::Request(request)).await;
        };

        let identity = match compute_canonical_invocation_identity(
            &ctx.client_pubkey,
            &request.method,
            request.params.as_ref(),
        ) {
            Ok(id) => id,
            Err(_) => return false,
        };
        let canonical_key = canonical_key_for(&identity);
        let lifecycle = self.lifecycle_from_context(ctx);
        let now = tokio::time::Instant::now();

        // Fast path: a live local entry already exists.
        if let Some(snapshot) = self.parked_snapshot(&canonical_key, now) {
            return self
                .handle_live_entry(&canonical_key, snapshot, ctx, &request, lifecycle, next)
                .await;
        }

        // No live local entry. Start a fresh payment flow for this request.
        self.start_payment_flow(
            ctx,
            request,
            capability,
            canonical_key,
            identity,
            lifecycle,
            next,
        )
        .await
    }

    fn parked_snapshot(
        &self,
        canonical_key: &str,
        now: tokio::time::Instant,
    ) -> Option<ParkedEntrySnapshot> {
        let parking = self.inner.parking.lock();
        parking.by_key.get(canonical_key).and_then(|entry| {
            if entry.expires_at > now {
                Some(ParkedEntrySnapshot::from(entry))
            } else {
                None
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_payment_flow(
        &self,
        ctx: &InboundContext,
        request: JsonRpcRequest,
        capability: PricedCapability,
        canonical_key: String,
        identity: CanonicalInvocationIdentity,
        lifecycle: PaymentLifecyclePolicy,
        next: Box<dyn PaymentNext>,
    ) -> bool {
        let now = tokio::time::Instant::now();

        match self
            .inner
            .auth_store
            .claim_or_set_pending(&identity, self.park_ttl_ms())
        {
            ClaimOrPending::Claimed => {
                // A grant from a previous settlement is still live. Forward immediately.
                next.set_canonical_invocation_id(canonical_key.clone());
                let result = next.run(JsonRpcMessage::Request(request)).await;
                self.remove_and_clear(&canonical_key).await;
                result
            }
            ClaimOrPending::AlreadyPending { remaining_ms } => {
                // Another request for the same canonical id is in flight.
                if lifecycle == PaymentLifecyclePolicy::Gating {
                    self.send_payment_pending(ctx, &request.id, remaining_ms)
                        .await;
                }
                next.release().await;
                false
            }
            ClaimOrPending::PendingSet => {
                // New gate event: park the request and emit a `PaymentGateRequest`.
                let params_json = match request.params.as_ref() {
                    Some(p) => serde_json::to_string(p).unwrap_or_default(),
                    None => String::new(),
                };
                let capability_name = self
                    .capability_name_for_request(&request)
                    .unwrap_or_else(|| capability.name.clone());

                next.set_canonical_invocation_id(canonical_key.clone());
                next.keep_alive();

                let nonce = self.inner.nonce.fetch_add(1, Ordering::SeqCst);
                let expires_at = now + self.park_ttl();
                let next_handle = Arc::new(Mutex::new(Some(next)));

                let entry = ParkedEntry {
                    identity,
                    request_event_id: ctx.request_event_id.clone(),
                    client_pubkey: ctx.client_pubkey.clone(),
                    mirrored_wrap_kind: ctx.mirrored_wrap_kind,
                    capability,
                    request: request.clone(),
                    next_and_message: Some((next_handle, JsonRpcMessage::Request(request))),
                    state: PaymentState::AwaitingInvoice,
                    expires_at,
                    nonce,
                    lifecycle,
                };

                // Atomic capacity check + insert under a single lock.
                let evicted = {
                    let mut parking = self.inner.parking.lock();
                    let mut evicted = None;
                    if parking.by_key.len() >= self.inner.config.parked_cap {
                        evicted = parking.evict_oldest();
                    }
                    parking.insert(&canonical_key, entry);
                    evicted
                };

                if let Some((_, evicted_entry)) = evicted {
                    self.release_and_reject_evicted(evicted_entry).await;
                }

                let event = PaymentGateRequest {
                    request_event_id: ctx.request_event_id.clone(),
                    client_pubkey: ctx.client_pubkey.clone(),
                    method: "tools/call".into(),
                    params_json,
                    capability_name,
                    canonical_invocation_id: canonical_key.clone(),
                };

                if self.inner.events_tx.try_send(event).is_err() {
                    // Queue overflow: remove the entry and fully release its Next.
                    self.remove_and_clear(&canonical_key).await;
                    return false;
                }

                tokio::spawn(self.clone().ttl_worker(canonical_key, nonce, expires_at));
                false
            }
            _ => {
                // Unknown future policy: do not park, just release the request.
                next.release().await;
                false
            }
        }
    }

    async fn handle_live_entry(
        &self,
        canonical_key: &str,
        snapshot: ParkedEntrySnapshot,
        ctx: &InboundContext,
        request: &JsonRpcRequest,
        lifecycle: PaymentLifecyclePolicy,
        next: Box<dyn PaymentNext>,
    ) -> bool {
        let now = tokio::time::Instant::now();
        match &snapshot.state {
            PaymentState::AwaitingInvoice
            | PaymentState::Publishing { .. }
            | PaymentState::InvoiceIssued { .. } => {
                // Duplicate before settlement. In gating mode answer with `-32043`; in
                // transparent mode the client already has or will receive a payment-required
                // notification for the original request.
                if lifecycle == PaymentLifecyclePolicy::Gating {
                    let remaining = self.remaining_ms(snapshot.expires_at, now);
                    self.send_payment_pending(ctx, &request.id, remaining).await;
                } else if let PaymentState::InvoiceIssued {
                    pay_req,
                    amount,
                    pmi,
                    ttl_secs,
                    description,
                } = &snapshot.state
                {
                    let _ = self
                        .send_payment_required_notification_data(
                            &snapshot.client_pubkey,
                            &snapshot.request_event_id,
                            snapshot.mirrored_wrap_kind,
                            *amount,
                            pay_req,
                            pmi,
                            *ttl_secs,
                            description.as_deref(),
                        )
                        .await;
                }

                // The duplicate is not forwarded; release its continuation so the
                // middleware chain can reclaim the route.
                next.release().await;
                false
            }
            PaymentState::Granted => {
                // A gating grant is waiting for the client to retry.  Try to claim it and
                // forward the *current* request.
                if !self.set_claiming(canonical_key) {
                    let remaining = self.park_ttl_ms();
                    if lifecycle == PaymentLifecyclePolicy::Gating {
                        self.send_payment_pending(ctx, &request.id, remaining).await;
                    }
                    next.release().await;
                    return false;
                }

                if self.inner.auth_store.claim(&snapshot.identity) {
                    next.set_canonical_invocation_id(canonical_key.to_string());
                    let result = next.run(JsonRpcMessage::Request(request.clone())).await;
                    self.remove_and_clear(canonical_key).await;
                    result
                } else {
                    // Grant was concurrently consumed or expired.  Clear the stale entry and
                    // start a fresh payment flow for this duplicate.
                    self.remove_and_clear(canonical_key).await;
                    self.start_payment_flow(
                        ctx,
                        request.clone(),
                        snapshot.capability.clone(),
                        canonical_key.to_string(),
                        snapshot.identity.clone(),
                        lifecycle,
                        next,
                    )
                    .await
                }
            }
            PaymentState::Claiming => {
                let remaining = self.remaining_ms(snapshot.expires_at, now);
                if lifecycle == PaymentLifecyclePolicy::Gating {
                    self.send_payment_pending(ctx, &request.id, remaining).await;
                }
                next.release().await;
                false
            }
        }
    }

    fn set_claiming(&self, canonical_key: &str) -> bool {
        let now = tokio::time::Instant::now();
        let mut parking = self.inner.parking.lock();
        if let Some(e) = parking.by_key.get_mut(canonical_key) {
            if matches!(e.state, PaymentState::Granted) {
                e.state = PaymentState::Claiming;
                e.expires_at = now + self.park_ttl();
                return true;
            }
        }
        false
    }

    fn remaining_ms(&self, expires_at: tokio::time::Instant, now: tokio::time::Instant) -> u64 {
        expires_at.saturating_duration_since(now).as_millis() as u64
    }

    fn match_priced_capability(&self, request: &JsonRpcRequest) -> Option<PricedCapability> {
        let name = request
            .params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())?;
        self.inner
            .config
            .priced_capabilities
            .iter()
            .find(|c| c.method == request.method && (c.name.is_empty() || c.name == name))
            .cloned()
    }

    fn capability_name_for_request(&self, request: &JsonRpcRequest) -> Option<String> {
        request
            .params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    fn lifecycle_from_context(&self, ctx: &InboundContext) -> PaymentLifecyclePolicy {
        match ctx.payment_interaction {
            Some(PaymentInteractionMode::ExplicitGating) => PaymentLifecyclePolicy::Gating,
            Some(PaymentInteractionMode::Transparent) => PaymentLifecyclePolicy::Transparent,
            None => self.inner.config.policy,
        }
    }

    fn max_invoice_ttl_secs(&self) -> u64 {
        let min_timeout = self
            .inner
            .config
            .request_timeout_secs
            .min(self.inner.config.session_timeout_secs);
        let min_needed = self.inner.config.execution_budget_secs + ROUTE_BUDGET_MARGIN_SECS;
        if min_timeout <= min_needed {
            0
        } else {
            // Strict `<` in `ttl + execution + margin < min_timeout`.
            min_timeout - min_needed - 1
        }
    }

    fn park_ttl(&self) -> Duration {
        Duration::from_secs(self.park_ttl_secs())
    }

    fn park_ttl_secs(&self) -> u64 {
        self.inner
            .config
            .payment_ttl_cap_secs
            .min(self.max_invoice_ttl_secs())
    }

    fn park_ttl_ms(&self) -> u64 {
        self.park_ttl().as_millis() as u64
    }

    async fn send_payment_required_error(
        &self,
        canonical_key: &str,
        expected_nonce: u64,
    ) -> Result<(), FfiError> {
        let (client_pubkey, request_event_id, request_id, option) = {
            let parking = self.inner.parking.lock();
            let Some(entry) = parking.by_key.get(canonical_key) else {
                return Err(payment_error("parked entry disappeared before response"));
            };
            if entry.nonce != expected_nonce {
                return Err(payment_error("invoice superseded before response"));
            }
            let (pay_req, amount, pmi, ttl_secs, description) = match &entry.state {
                PaymentState::InvoiceIssued {
                    pay_req,
                    amount,
                    pmi,
                    ttl_secs,
                    description,
                }
                | PaymentState::Publishing {
                    pay_req,
                    amount,
                    pmi,
                    ttl_secs,
                    description,
                } => (
                    pay_req.clone(),
                    *amount,
                    pmi.clone(),
                    *ttl_secs,
                    description.clone(),
                ),
                _ => return Err(payment_error("invoice state missing before response")),
            };
            let option = PaymentOption {
                amount,
                pmi,
                pay_req,
                description,
                ttl: Some(ttl_secs),
                meta: None,
            };
            (
                entry.client_pubkey.clone(),
                entry.request_event_id.clone(),
                entry.request.id.clone(),
                option,
            )
        };

        let response = build_payment_required_error(request_id, option);
        self.inner
            .transport
            .send_targeted_response(
                client_pubkey,
                request_event_id,
                JsonRpcMessage::ErrorResponse(response),
            )
            .await
            .map_err(|e| payment_error(format!("failed to publish payment_required: {e}")))
    }

    async fn send_payment_required_notification(
        &self,
        canonical_key: &str,
        expected_nonce: u64,
    ) -> Result<(), FfiError> {
        let (
            client_pubkey,
            request_event_id,
            mirrored_wrap_kind,
            amount,
            pay_req,
            pmi,
            ttl,
            description,
        ) = {
            let parking = self.inner.parking.lock();
            let Some(entry) = parking.by_key.get(canonical_key) else {
                return Err(payment_error(
                    "parked entry disappeared before notification",
                ));
            };
            if entry.nonce != expected_nonce {
                return Err(payment_error("invoice superseded before notification"));
            }
            let (pay_req, amount, pmi, ttl_secs, description) = match &entry.state {
                PaymentState::InvoiceIssued {
                    pay_req,
                    amount,
                    pmi,
                    ttl_secs,
                    description,
                }
                | PaymentState::Publishing {
                    pay_req,
                    amount,
                    pmi,
                    ttl_secs,
                    description,
                } => (
                    pay_req.clone(),
                    *amount,
                    pmi.clone(),
                    *ttl_secs,
                    description.clone(),
                ),
                _ => return Err(payment_error("invoice state missing before notification")),
            };
            (
                entry.client_pubkey.clone(),
                entry.request_event_id.clone(),
                entry.mirrored_wrap_kind,
                amount,
                pay_req,
                pmi,
                ttl_secs,
                description,
            )
        };

        self.send_payment_required_notification_data(
            &client_pubkey,
            &request_event_id,
            mirrored_wrap_kind,
            amount,
            &pay_req,
            &pmi,
            ttl,
            description.as_deref(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_payment_required_notification_data(
        &self,
        client_pubkey: &str,
        request_event_id: &str,
        mirrored_wrap_kind: Option<u16>,
        amount: i64,
        pay_req: &str,
        pmi: &str,
        ttl_secs: u64,
        description: Option<&str>,
    ) -> Result<(), FfiError> {
        let params = PaymentRequiredParams {
            amount,
            pay_req: pay_req.into(),
            pmi: pmi.into(),
            description: description.map(String::from),
            ttl: Some(ttl_secs),
            meta: None,
        };
        let notification = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: PAYMENT_REQUIRED_METHOD.into(),
            params: Some(serde_json::to_value(&params).expect("serialize")),
        });
        self.inner
            .transport
            .send_payment_notification(
                client_pubkey.into(),
                request_event_id.into(),
                mirrored_wrap_kind,
                notification,
            )
            .await
            .map_err(|e| payment_error(format!("failed to publish payment_required: {e}")))
    }

    async fn send_payment_pending(
        &self,
        ctx: &InboundContext,
        request_id: &Value,
        remaining_ms: u64,
    ) {
        let response = build_payment_pending_error(request_id.clone(), remaining_ms);
        let _ = self
            .inner
            .transport
            .send_targeted_response(
                ctx.client_pubkey.clone(),
                ctx.request_event_id.clone(),
                JsonRpcMessage::ErrorResponse(response),
            )
            .await;
    }

    async fn send_payment_rejected(
        &self,
        client_pubkey: &str,
        request_event_id: &str,
        mirrored_wrap_kind: Option<u16>,
        pmi: &str,
        amount: Option<i64>,
        message: Option<&str>,
    ) {
        let params = PaymentRejectedParams {
            pmi: pmi.into(),
            amount,
            message: message.map(String::from),
        };
        let notification = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: PAYMENT_REJECTED_METHOD.into(),
            params: Some(serde_json::to_value(&params).expect("serialize")),
        });
        let _ = self
            .inner
            .transport
            .send_payment_notification(
                client_pubkey.into(),
                request_event_id.into(),
                mirrored_wrap_kind,
                notification,
            )
            .await;
    }

    async fn remove_and_clear(&self, canonical_key: &str) -> Option<ParkedEntry> {
        let (entry, next) = {
            let mut parking = self.inner.parking.lock();
            let mut entry = parking.remove(canonical_key)?;
            self.inner.auth_store.clear_pending(&entry.identity);
            let next = entry.next_and_message.take().and_then(|(handle, _)| {
                let mut guard = handle.lock();
                guard.take()
            });
            (entry, next)
        };

        if let Some(next) = next {
            next.release().await;
        }

        Some(entry)
    }

    async fn release_and_reject_evicted(&self, mut entry: ParkedEntry) {
        if let Some((handle, _)) = entry.next_and_message.take() {
            let next = {
                let mut guard = handle.lock();
                guard.take()
            };
            if let Some(next) = next {
                next.release().await;
            }
        }
        self.inner.auth_store.clear_pending(&entry.identity);

        if entry.lifecycle == PaymentLifecyclePolicy::Transparent {
            let (pmi, amount) = payment_rejection_details(&entry);
            self.send_payment_rejected(
                &entry.client_pubkey,
                &entry.request_event_id,
                entry.mirrored_wrap_kind,
                pmi,
                Some(amount),
                Some("parked capacity exceeded"),
            )
            .await;
        }
    }

    async fn ttl_worker(self, key: String, nonce: u64, expires_at: tokio::time::Instant) {
        tokio::time::sleep_until(expires_at).await;
        self.expire(&key, nonce).await;
    }

    async fn expire(&self, key: &str, nonce: u64) {
        let now = tokio::time::Instant::now();

        let mut entry = {
            let mut parking = self.inner.parking.lock();
            let Some(e) = parking.by_key.get(key) else {
                return;
            };
            if e.nonce != nonce || e.expires_at > now {
                return;
            }
            parking.remove(key).expect("entry")
        };

        self.inner.auth_store.clear_pending(&entry.identity);

        if entry.lifecycle == PaymentLifecyclePolicy::Transparent {
            let (pmi, amount) = payment_rejection_details(&entry);
            self.send_payment_rejected(
                &entry.client_pubkey,
                &entry.request_event_id,
                entry.mirrored_wrap_kind,
                pmi,
                Some(amount),
                Some("payment window expired"),
            )
            .await;
        }

        if let Some((handle, _)) = entry.next_and_message.take() {
            let next = {
                let mut guard = handle.lock();
                guard.take()
            };
            if let Some(next) = next {
                next.release().await;
            }
        }
    }
}

#[async_trait]
impl InboundMiddleware for PaymentGate {
    async fn handle(&self, message: JsonRpcMessage, ctx: &InboundContext, next: Next) -> bool {
        let next: Box<dyn PaymentNext> = Box::new(SdkNext(next));
        self.handle_inner(message, ctx, next).await
    }
}

impl Parking {
    fn insert(&mut self, key: &str, entry: ParkedEntry) {
        self.by_event
            .insert(entry.request_event_id.clone(), key.to_string());
        if let PaymentState::InvoiceIssued { pay_req, .. } = &entry.state {
            self.by_payreq.insert(pay_req.clone(), key.to_string());
        }
        self.order.push_back(key.to_string());
        self.by_key.insert(key.to_string(), entry);
    }

    fn remove(&mut self, key: &str) -> Option<ParkedEntry> {
        let entry = self.by_key.remove(key)?;
        self.by_event.remove(&entry.request_event_id);
        if let PaymentState::InvoiceIssued { pay_req, .. } = &entry.state {
            self.by_payreq.remove(pay_req);
        }
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        Some(entry)
    }

    fn evict_oldest(&mut self) -> Option<(String, ParkedEntry)> {
        let key = self.order.pop_front()?;
        let entry = self.by_key.remove(&key)?;
        self.by_event.remove(&entry.request_event_id);
        if let PaymentState::InvoiceIssued { pay_req, .. } = &entry.state {
            self.by_payreq.remove(pay_req);
        }
        Some((key, entry))
    }
}

/// Outcome of preparing a `mark_settled` call.
#[allow(clippy::large_enum_variant)]
enum SettleOutcome {
    Transparent {
        identity: CanonicalInvocationIdentity,
        next_and_message: Option<(Box<dyn PaymentNext>, JsonRpcMessage)>,
        client_pubkey: String,
        request_event_id: String,
        mirrored_wrap_kind: Option<u16>,
        amount: i64,
        pmi: String,
        ttl_ms: u64,
    },
    Gating {
        canonical_key: String,
        nonce: u64,
        expires_at: tokio::time::Instant,
    },
}

type InvoicePrepResult = (
    String,
    u64,
    tokio::time::Instant,
    PaymentLifecyclePolicy,
    Option<NextHandle>,
);

impl PaymentGate {
    fn prepare_invoice(
        &self,
        request_event_id: &str,
        amount_sats: i64,
        pay_req: &str,
        pmi: &str,
        ttl_secs: u64,
        description: Option<&str>,
    ) -> Result<InvoicePrepResult, FfiError> {
        let now = tokio::time::Instant::now();
        let canonical_key = {
            let parking = self.inner.parking.lock();
            parking
                .by_event
                .get(request_event_id)
                .cloned()
                .ok_or_else(|| payment_error("unknown request_event_id"))?
        };

        let mut parking = self.inner.parking.lock();
        let Some(mut entry) = parking.remove(&canonical_key) else {
            return Err(payment_error("parked entry disappeared"));
        };

        // Validate amount against the advertised capability.
        if amount_sats < entry.capability.amount_sats {
            parking.insert(&canonical_key, entry);
            return Err(validation_error("amount below capability minimum"));
        }
        if let Some(max) = entry.capability.max_amount_sats {
            if amount_sats > max {
                parking.insert(&canonical_key, entry);
                return Err(validation_error("amount above capability maximum"));
            }
        }

        // Validate PMI and TTL.
        if pmi != PMI_BITCOIN_LIGHTNING_BOLT11 {
            parking.insert(&canonical_key, entry);
            return Err(validation_error("unsupported payment method identifier"));
        }
        if ttl_secs == 0
            || ttl_secs > self.inner.config.payment_ttl_cap_secs
            || ttl_secs > self.max_invoice_ttl_secs()
        {
            parking.insert(&canonical_key, entry);
            return Err(validation_error("ttl violates payment or route budget cap"));
        }

        let nonce = self.inner.nonce.fetch_add(1, Ordering::SeqCst);
        let expires_at = now + Duration::from_secs(ttl_secs);

        match &entry.state {
            PaymentState::AwaitingInvoice => {}
            PaymentState::Publishing { .. } => {
                // Coalesce concurrent submissions: an attempt is already in
                // flight for this identity. Replacing its nonce here would let
                // a fast failure roll back a slow success whose invoice the
                // client already received (lost-success race, round-4 review).
                // The consumer treats this error as retryable and re-binds the
                // same invoice on the next gate event.
                parking.insert(&canonical_key, entry);
                return Err(payment_error(
                    "invoice publication already in flight; retry on the next gate event",
                ));
            }
            PaymentState::InvoiceIssued { .. } | PaymentState::Granted | PaymentState::Claiming => {
                parking.insert(&canonical_key, entry);
                return Err(payment_error(
                    "invoice already issued or payment already settled",
                ));
            }
        }

        // Mark this particular attempt as in flight. The actual `InvoiceIssued`
        // commit only happens after the transport send succeeds, and only if this
        // nonce still owns the state.
        entry.state = PaymentState::Publishing {
            pay_req: pay_req.to_string(),
            amount: amount_sats,
            pmi: pmi.to_string(),
            ttl_secs,
            description: description.map(String::from),
        };

        entry.nonce = nonce;
        entry.expires_at = expires_at;
        self.inner
            .auth_store
            .update_pending_ttl(&entry.identity, ttl_secs * 1000);

        // In gating mode the original request must be released after the
        // targeted response is sent. The `Next` stays in the entry's shared
        // handle; `submit_invoice` will consume it on success. This clone lets a
        // failed or superseded attempt release the request only if no in-flight
        // attempt still needs it.
        let next_handle = if entry.lifecycle == PaymentLifecyclePolicy::Gating {
            entry
                .next_and_message
                .as_ref()
                .map(|(handle, _)| handle.clone())
        } else {
            None
        };
        let lifecycle = entry.lifecycle;
        parking.insert(&canonical_key, entry);
        drop(parking);

        Ok((canonical_key, nonce, expires_at, lifecycle, next_handle))
    }

    /// Promote a `Publishing` state to `InvoiceIssued` only if `nonce` still
    /// owns the in-flight attempt. Returns `true` when the commit happened, in
    /// which case the `pay_req` mapping has also been registered for settlement.
    fn finalize_invoice(&self, canonical_key: &str, nonce: u64) -> bool {
        let mut parking = self.inner.parking.lock();
        let Some(entry) = parking.by_key.get_mut(canonical_key) else {
            return false;
        };
        if !matches!(entry.state, PaymentState::Publishing { .. }) || entry.nonce != nonce {
            return false;
        }
        let (pay_req, amount, pmi, ttl_secs, description) = match &entry.state {
            PaymentState::Publishing {
                pay_req,
                amount,
                pmi,
                ttl_secs,
                description,
            } => (
                pay_req.clone(),
                *amount,
                pmi.clone(),
                *ttl_secs,
                description.clone(),
            ),
            _ => return false,
        };
        entry.state = PaymentState::InvoiceIssued {
            pay_req: pay_req.clone(),
            amount,
            pmi,
            ttl_secs,
            description,
        };
        parking.by_payreq.insert(pay_req, canonical_key.to_string());
        true
    }

    /// Rollback an invoice transition when the transport publish fails.
    ///
    /// Only the `Publishing` state owned by this `nonce` is reverted to
    /// `AwaitingInvoice`. If a newer attempt is in flight, the state is left
    /// untouched; if a newer attempt has already committed to `InvoiceIssued`,
    /// this failed attempt releases its copy of the parked `Next` so the
    /// successful response is not left with a dangling request.
    async fn rollback_invoice(
        &self,
        canonical_key: &str,
        _lifecycle: PaymentLifecyclePolicy,
        next_handle: Option<NextHandle>,
        nonce: u64,
    ) {
        let now = tokio::time::Instant::now();
        let expires_at = now + self.park_ttl();

        let (identity, should_release_next) = {
            let mut parking = self.inner.parking.lock();
            if let Some(entry) = parking.by_key.get_mut(canonical_key) {
                match &entry.state {
                    PaymentState::Publishing { .. } if entry.nonce == nonce => {
                        entry.state = PaymentState::AwaitingInvoice;
                        entry.expires_at = expires_at;
                        (Some(entry.identity.clone()), false)
                    }
                    PaymentState::Publishing { .. } => {
                        // Another attempt is in flight and owns the state; leave
                        // the `Next` for it.
                        (None, false)
                    }
                    PaymentState::InvoiceIssued { .. }
                    | PaymentState::Granted
                    | PaymentState::Claiming => {
                        // A newer attempt won. If this attempt holds the only
                        // remaining `Next` handle, release the request now.
                        (None, true)
                    }
                    PaymentState::AwaitingInvoice => (None, false),
                }
            } else {
                // The entry was removed; release any request we still hold.
                (None, true)
            }
        };

        if let Some(identity) = identity {
            self.inner
                .auth_store
                .update_pending_ttl(&identity, self.park_ttl_ms());
        }

        if should_release_next {
            if let Some(handle) = next_handle {
                let next = {
                    let mut guard = handle.lock();
                    guard.take()
                };
                if let Some(next) = next {
                    next.release().await;
                }
            }
        }

        tokio::spawn(
            self.clone()
                .ttl_worker(canonical_key.to_string(), nonce, expires_at),
        );
    }

    async fn prepare_settle(&self, pay_req: &str) -> Result<SettleOutcome, FfiError> {
        let now = tokio::time::Instant::now();
        let canonical_key = {
            let parking = self.inner.parking.lock();
            parking
                .by_payreq
                .get(pay_req)
                .cloned()
                .ok_or_else(|| payment_error("unknown pay_req"))?
        };

        let mut entry = {
            let mut parking = self.inner.parking.lock();
            let Some(e) = parking.remove(&canonical_key) else {
                return Err(payment_error("parked entry disappeared"));
            };
            // Remove the pay_req mapping now; the same invoice cannot settle twice.
            parking.by_payreq.remove(pay_req);
            e
        };

        let (amount, pmi, ttl_secs) = match entry.state.clone() {
            PaymentState::InvoiceIssued {
                amount,
                pmi,
                ttl_secs,
                ..
            } => (amount, pmi, ttl_secs),
            _ => return Err(payment_error("no outstanding invoice for pay_req")),
        };

        let identity = entry.identity.clone();
        let ttl_ms = ttl_secs * 1000;

        if entry.lifecycle == PaymentLifecyclePolicy::Transparent {
            // Remove the parked entry from lookup maps and hand the Next to mark_settled.
            self.inner.auth_store.clear_pending(&identity);
            let client_pubkey = entry.client_pubkey.clone();
            let request_event_id = entry.request_event_id.clone();
            let mirrored_wrap_kind = entry.mirrored_wrap_kind;
            let next_and_message = match entry.next_and_message.take() {
                Some((handle, message)) => {
                    let next = {
                        let mut guard = handle.lock();
                        guard.take()
                    };
                    next.map(|next| (next, message))
                }
                None => None,
            };

            Ok(SettleOutcome::Transparent {
                identity,
                next_and_message,
                client_pubkey,
                request_event_id,
                mirrored_wrap_kind,
                amount,
                pmi,
                ttl_ms,
            })
        } else {
            // Gating: store the grant and stay parked (no Next). A fresh nonce/expires
            // lets the TTL worker keep the grant window bounded.
            let nonce = self.inner.nonce.fetch_add(1, Ordering::SeqCst);
            let expires_at = now + Duration::from_secs(ttl_secs);

            // Release the parked Next, if any, before returning. Gating already
            // answered `-32042` in `submit_invoice`, so this is just defensive cleanup.
            if let Some((handle, _)) = entry.next_and_message.take() {
                let next = {
                    let mut guard = handle.lock();
                    guard.take()
                };
                if let Some(next) = next {
                    next.release().await;
                }
            }

            self.inner.auth_store.clear_pending(&identity);
            self.inner.auth_store.grant(&identity, ttl_ms);

            let mut parking = self.inner.parking.lock();
            entry.state = PaymentState::Granted;
            entry.nonce = nonce;
            entry.expires_at = expires_at;
            entry.next_and_message = None;
            parking.insert(&canonical_key, entry);
            drop(parking);

            Ok(SettleOutcome::Gating {
                canonical_key,
                nonce,
                expires_at,
            })
        }
    }

    async fn prepare_replay(
        &self,
        request_event_id: &str,
    ) -> Result<Option<(Box<dyn PaymentNext>, JsonRpcMessage)>, FfiError> {
        let canonical_key = {
            let parking = self.inner.parking.lock();
            parking
                .by_event
                .get(request_event_id)
                .cloned()
                .ok_or_else(|| payment_error("unknown request_event_id"))?
        };

        let Some(mut entry) = ({
            let mut parking = self.inner.parking.lock();
            parking.remove(&canonical_key)
        }) else {
            return Err(payment_error("parked entry disappeared"));
        };

        self.inner.auth_store.clear_pending(&entry.identity);
        let next_and_message = match entry.next_and_message.take() {
            Some((handle, message)) => {
                let next = {
                    let mut guard = handle.lock();
                    guard.take()
                };
                next.map(|next| (next, message))
            }
            None => None,
        };

        match entry.state {
            PaymentState::AwaitingInvoice => Ok(next_and_message),
            _ => Err(payment_error(
                "request already settled, invoiced, or expired",
            )),
        }
    }
}

fn canonical_key_for(identity: &CanonicalInvocationIdentity) -> String {
    format!("{}:{}", identity.client_pubkey, identity.invocation_hash)
}

fn payment_rejection_details(entry: &ParkedEntry) -> (&str, i64) {
    match &entry.state {
        PaymentState::InvoiceIssued { pmi, amount, .. }
        | PaymentState::Publishing { pmi, amount, .. } => (pmi.as_str(), *amount),
        _ => (PMI_BITCOIN_LIGHTNING_BOLT11, entry.capability.amount_sats),
    }
}

fn parse_meta_json(meta_json: Option<&str>) -> Result<Option<Meta>, FfiError> {
    let Some(text) = meta_json else {
        return Ok(None);
    };
    if text.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(text).map_err(|e| FfiError {
        code: ErrorCode::Serialization,
        message: format!("invalid meta_json: {e}"),
    })?;
    match value {
        Value::Object(map) if !map.is_empty() => Ok(Some(map)),
        _ => Ok(None),
    }
}

fn validation_error(msg: impl Into<String>) -> FfiError {
    FfiError {
        code: ErrorCode::Validation,
        message: msg.into(),
    }
}

fn payment_error(msg: impl Into<String>) -> FfiError {
    FfiError {
        code: ErrorCode::Payment,
        message: msg.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contextvm_sdk::core::types::{
        JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    };
    use contextvm_sdk::payments::constants::{
        PAYMENT_ACCEPTED_METHOD, PAYMENT_PENDING_ERROR_CODE, PAYMENT_REJECTED_METHOD,
        PAYMENT_REQUIRED_ERROR_CODE, PAYMENT_REQUIRED_METHOD,
    };
    use contextvm_sdk::transport::server::InboundContext;
    use std::sync::atomic::AtomicUsize;
    use tokio_util::sync::CancellationToken;

    fn test_config(policy: PaymentLifecyclePolicy) -> PaymentGateConfig {
        PaymentGateConfig {
            payment_ttl_cap_secs: 10,
            execution_budget_secs: 2,
            request_timeout_secs: 30,
            session_timeout_secs: 30,
            parked_cap: 64,
            event_queue_bound: 8,
            policy,
            priced_capabilities: vec![PricedCapability {
                method: "tools/call".into(),
                name: "echo".into(),
                amount_sats: 1000,
                max_amount_sats: Some(2000),
                currency_unit: "sats".into(),
                description: "Echo capability".into(),
            }],
        }
    }

    fn make_context(
        client_pubkey: &str,
        request_event_id: &str,
        payment_interaction: Option<PaymentInteractionMode>,
    ) -> InboundContext {
        InboundContext::new(
            client_pubkey,
            request_event_id,
            false,
            None,
            None,
            payment_interaction,
            CancellationToken::new(),
        )
    }

    fn tools_call(id: &str, name: &str) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(id),
            method: "tools/call".into(),
            params: Some(serde_json::json!({"name": name, "arguments": {}})),
        })
    }

    fn tools_call_extra(id: &str, name: &str, extra: serde_json::Value) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(id),
            method: "tools/call".into(),
            params: Some(serde_json::json!({"name": name, "arguments": {}, "extra": extra})),
        })
    }

    fn tools_list(id: &str) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(id),
            method: "tools/list".into(),
            params: None,
        })
    }

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    enum TransportRecord {
        Notification {
            client_pubkey: String,
            request_event_id: String,
            mirrored_wrap_kind: Option<u16>,
            notification: JsonRpcMessage,
        },
        Response {
            client_pubkey: String,
            request_event_id: String,
            response: JsonRpcMessage,
        },
    }

    #[derive(Clone)]
    struct FakeTransport {
        inner: Arc<std::sync::Mutex<Vec<TransportRecord>>>,
        fail_next_notification: Arc<AtomicUsize>,
        fail_next_response: Arc<AtomicUsize>,
        /// When > 0, every notification send sleeps this many (possibly paused)
        /// milliseconds before completing — used to force slow-success publishes.
        notification_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    }

    impl FakeTransport {
        fn new() -> Self {
            Self {
                inner: Arc::new(std::sync::Mutex::new(Vec::new())),
                fail_next_notification: Arc::new(AtomicUsize::new(0)),
                fail_next_response: Arc::new(AtomicUsize::new(0)),
                notification_delay_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            }
        }

        fn records(&self) -> Vec<TransportRecord> {
            self.inner.lock().unwrap().clone()
        }

        fn fail_next_notification(&self, count: usize) {
            self.fail_next_notification.store(count, Ordering::SeqCst);
        }

        fn fail_next_response(&self, count: usize) {
            self.fail_next_response.store(count, Ordering::SeqCst);
        }

        fn set_notification_delay_ms(&self, ms: u64) {
            self.notification_delay_ms.store(ms, Ordering::SeqCst);
        }
    }

    impl PaymentGateTransport for FakeTransport {
        fn send_payment_notification(
            &self,
            client_pubkey: String,
            request_event_id: String,
            mirrored_wrap_kind: Option<u16>,
            notification: JsonRpcMessage,
        ) -> Pin<Box<dyn Future<Output = contextvm_sdk::Result<()>> + Send>> {
            let inner = self.inner.clone();
            let counter = self.fail_next_notification.clone();
            let delay_ms = self.notification_delay_ms.load(Ordering::SeqCst);
            Box::pin(async move {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                if counter
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                        if v > 0 {
                            Some(v - 1)
                        } else {
                            None
                        }
                    })
                    .is_ok()
                {
                    return Err(contextvm_sdk::Error::Transport(
                        "injected notification failure".into(),
                    ));
                }
                inner.lock().unwrap().push(TransportRecord::Notification {
                    client_pubkey,
                    request_event_id,
                    mirrored_wrap_kind,
                    notification,
                });
                Ok(())
            })
        }

        fn send_targeted_response(
            &self,
            client_pubkey: String,
            request_event_id: String,
            response: JsonRpcMessage,
        ) -> Pin<Box<dyn Future<Output = contextvm_sdk::Result<()>> + Send>> {
            let inner = self.inner.clone();
            let counter = self.fail_next_response.clone();
            let delay_ms = self.notification_delay_ms.load(Ordering::SeqCst);
            Box::pin(async move {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                if counter
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                        if v > 0 {
                            Some(v - 1)
                        } else {
                            None
                        }
                    })
                    .is_ok()
                {
                    return Err(contextvm_sdk::Error::Transport(
                        "injected response failure".into(),
                    ));
                }
                inner.lock().unwrap().push(TransportRecord::Response {
                    client_pubkey,
                    request_event_id,
                    response,
                });
                Ok(())
            })
        }
    }

    #[derive(Clone)]
    struct FakeNext(Arc<std::sync::Mutex<Option<JsonRpcMessage>>>);

    impl FakeNext {
        fn new() -> (Self, Arc<std::sync::Mutex<Option<JsonRpcMessage>>>) {
            let shared = Arc::new(std::sync::Mutex::new(None));
            (Self(shared.clone()), shared)
        }
    }

    impl PaymentNext for FakeNext {
        fn keep_alive(&self) {}

        fn set_canonical_invocation_id(&self, _id: String) {}

        fn run(
            self: Box<Self>,
            message: JsonRpcMessage,
        ) -> Pin<Box<dyn Future<Output = bool> + Send>> {
            Box::pin(async move {
                *self.0.lock().unwrap() = Some(message);
                true
            })
        }

        fn release(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async {})
        }
    }

    fn boxed_next(recorder: Arc<std::sync::Mutex<Option<JsonRpcMessage>>>) -> Box<dyn PaymentNext> {
        Box::new(FakeNext(recorder))
    }

    fn find_notification(records: &[TransportRecord], method: &str) -> Option<JsonRpcNotification> {
        records.iter().find_map(|r| match r {
            TransportRecord::Notification {
                notification: JsonRpcMessage::Notification(n),
                ..
            } if n.method == method => Some(n.clone()),
            _ => None,
        })
    }

    fn find_response(records: &[TransportRecord]) -> Option<JsonRpcErrorResponse> {
        records.iter().find_map(|r| match r {
            TransportRecord::Response {
                response: JsonRpcMessage::ErrorResponse(e),
                ..
            } => Some(e.clone()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn unpriced_method_bypasses_gate() {
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Transparent),
            Arc::new(FakeTransport::new()),
        )
        .unwrap();
        let (_, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);
        let forwarded = gate
            .handle_inner(tools_list("1"), &ctx, boxed_next(recorder.clone()))
            .await;
        assert!(forwarded);
        assert!(recorder.lock().unwrap().is_some());
        assert!(gate.try_recv().is_none());
    }

    #[tokio::test]
    async fn unmatched_capability_bypasses_gate() {
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Transparent),
            Arc::new(FakeTransport::new()),
        )
        .unwrap();
        let (_, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);
        let forwarded = gate
            .handle_inner(tools_call("2", "unknown"), &ctx, boxed_next(recorder))
            .await;
        assert!(forwarded);
        assert!(gate.try_recv().is_none());
    }

    #[tokio::test]
    async fn transparent_park_and_settle() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Transparent),
            transport.clone(),
        )
        .unwrap();
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);

        // First call is parked.
        let forwarded = gate
            .handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        assert!(!forwarded);
        let event = gate.try_recv().expect("event emitted");
        assert_eq!(event.request_event_id, "e1");

        // W1c provides an invoice.
        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc...",
            "bitcoin-lightning-bolt11",
            10,
            Some("pay me"),
        )
        .await
        .unwrap();

        let required = find_notification(&transport.records(), PAYMENT_REQUIRED_METHOD)
            .expect("payment_required notification");
        let params: PaymentRequiredParams =
            serde_json::from_value(required.params.unwrap()).unwrap();
        assert_eq!(params.amount, 1000);
        assert_eq!(params.pay_req, "lnbc...");

        // Client settles.
        gate.mark_settled("lnbc...", None).await.unwrap();

        let accepted = find_notification(&transport.records(), PAYMENT_ACCEPTED_METHOD)
            .expect("payment_accepted notification");
        let _params: PaymentAcceptedParams =
            serde_json::from_value(accepted.params.unwrap()).unwrap();

        // The original parked Next was forwarded.
        assert_eq!(
            recorder.lock().unwrap().as_ref().map(|m| match m {
                JsonRpcMessage::Request(r) => r.method.clone(),
                _ => String::new(),
            }),
            Some("tools/call".into())
        );

        // Identical call is a new payment event: settled state is not a reusable
        // free-authorization, so it should park again and emit a fresh gate request.
        let (_next3, recorder3) = FakeNext::new();
        let ctx2 = make_context("client", "e2", None);
        let forwarded = gate
            .handle_inner(
                tools_call("3", "echo"),
                &ctx2,
                boxed_next(recorder3.clone()),
            )
            .await;
        assert!(!forwarded);
        assert!(gate.try_recv().is_some());
    }

    #[tokio::test]
    async fn gating_park_and_settle() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Gating),
            transport.clone(),
        )
        .unwrap();
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", Some(PaymentInteractionMode::ExplicitGating));

        let forwarded = gate
            .handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        assert!(!forwarded);
        let event = gate.try_recv().expect("event emitted");

        // No notification, but W1c can submit an invoice.
        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc...",
            "bitcoin-lightning-bolt11",
            10,
            None,
        )
        .await
        .unwrap();

        let response =
            find_response(&transport.records()).expect("targeted payment_required error");
        assert_eq!(response.error.code, PAYMENT_REQUIRED_ERROR_CODE);

        // Client settles.
        gate.mark_settled("lnbc...", None).await.unwrap();

        // Client retries with a new request (same identity).
        let (_next2, recorder2) = FakeNext::new();
        let ctx2 = make_context("client", "e2", Some(PaymentInteractionMode::ExplicitGating));
        let forwarded = gate
            .handle_inner(
                tools_call("2", "echo"),
                &ctx2,
                boxed_next(recorder2.clone()),
            )
            .await;
        assert!(forwarded);
        assert!(gate.try_recv().is_none());
    }

    #[tokio::test]
    async fn gating_duplicate_before_invoice_is_pending() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Gating),
            transport.clone(),
        )
        .unwrap();
        let ctx = make_context("client", "e1", Some(PaymentInteractionMode::ExplicitGating));

        let (_next, recorder) = FakeNext::new();
        let forwarded = gate
            .handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        assert!(!forwarded);

        // Duplicate before submit_invoice yields -32043.
        let (_next2, recorder2) = FakeNext::new();
        let forwarded = gate
            .handle_inner(tools_call("2", "echo"), &ctx, boxed_next(recorder2.clone()))
            .await;
        assert!(!forwarded);

        let response = find_response(&transport.records()).expect("targeted payment_pending error");
        assert_eq!(response.error.code, PAYMENT_PENDING_ERROR_CODE);
    }

    #[tokio::test]
    async fn transparent_mark_replayed_forwards_current_request() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Transparent),
            transport.clone(),
        )
        .unwrap();
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);

        let forwarded = gate
            .handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        assert!(!forwarded);
        let event = gate.try_recv().unwrap();

        // Foreign consumer already has a cached result; forward the current request.
        gate.mark_replayed(&event.request_event_id).await.unwrap();

        assert_eq!(
            recorder.lock().unwrap().as_ref().map(|m| match m {
                JsonRpcMessage::Request(r) => r.method.clone(),
                _ => String::new(),
            }),
            Some("tools/call".into())
        );

        // A subsequent identical request is a new payment event, not a free replay.
        let (_next2, recorder2) = FakeNext::new();
        let ctx2 = make_context("client", "e2", None);
        let forwarded = gate
            .handle_inner(
                tools_call("2", "echo"),
                &ctx2,
                boxed_next(recorder2.clone()),
            )
            .await;
        assert!(!forwarded);
        assert!(gate.try_recv().is_some());
        assert!(find_notification(&transport.records(), PAYMENT_REQUIRED_METHOD).is_none());
    }

    #[tokio::test]
    async fn mark_failed_clears_invoice() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Transparent),
            transport.clone(),
        )
        .unwrap();
        let (_next, _recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);

        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(_recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc...",
            "bitcoin-lightning-bolt11",
            10,
            None,
        )
        .await
        .unwrap();

        gate.mark_failed("lnbc...", "invoice expired")
            .await
            .unwrap();

        let rejected = find_notification(&transport.records(), PAYMENT_REJECTED_METHOD)
            .expect("payment_rejected");
        let _params: PaymentRejectedParams =
            serde_json::from_value(rejected.params.unwrap()).unwrap();

        // A retry starts a fresh payment flow.
        let (_next2, recorder2) = FakeNext::new();
        let forwarded = gate
            .handle_inner(tools_call("2", "echo"), &ctx, boxed_next(recorder2.clone()))
            .await;
        assert!(!forwarded);
        assert!(gate.try_recv().is_some());
    }

    #[tokio::test]
    async fn ttl_expires_and_rejects() {
        tokio::time::pause();
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Transparent),
            transport.clone(),
        )
        .unwrap();
        let (_next, _recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);

        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(_recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc...",
            "bitcoin-lightning-bolt11",
            10,
            None,
        )
        .await
        .unwrap();

        // Advance past the 10-second invoice TTL.
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;

        let rejected = find_notification(&transport.records(), PAYMENT_REJECTED_METHOD)
            .expect("payment_rejected after expiry");
        let _params: PaymentRejectedParams =
            serde_json::from_value(rejected.params.unwrap()).unwrap();

        // A retry starts fresh.
        let (_next2, recorder2) = FakeNext::new();
        let forwarded = gate
            .handle_inner(tools_call("2", "echo"), &ctx, boxed_next(recorder2.clone()))
            .await;
        assert!(!forwarded);
        assert!(gate.try_recv().is_some());
    }

    #[tokio::test]
    async fn parked_capacity_eviction() {
        let transport = Arc::new(FakeTransport::new());
        let mut config = test_config(PaymentLifecyclePolicy::Transparent);
        config.parked_cap = 2;
        let gate = PaymentGate::new(config, transport.clone()).unwrap();

        for i in 0..3 {
            let (_next, recorder) = FakeNext::new();
            let ctx = make_context("client", &format!("e{i}"), None);
            gate.handle_inner(
                tools_call_extra(&format!("{i}"), "echo", serde_json::json!({"i": i})),
                &ctx,
                boxed_next(recorder.clone()),
            )
            .await;
        }

        // The oldest parked entry was evicted and a payment_rejected notification was sent.
        let records = transport.records();
        let rejected_count = records
            .iter()
            .filter(|r| matches!(r, TransportRecord::Notification { notification, .. } if matches!(notification, JsonRpcMessage::Notification(n) if n.method == PAYMENT_REJECTED_METHOD)))
            .count();
        assert_eq!(rejected_count, 1);
    }

    #[tokio::test]
    async fn event_queue_overflow_rolls_back() {
        let transport = Arc::new(FakeTransport::new());
        let mut config = test_config(PaymentLifecyclePolicy::Transparent);
        config.event_queue_bound = 1;
        let gate = PaymentGate::new(config, transport.clone()).unwrap();

        // First request fills the queue.
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);
        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;

        // Second request (different identity) cannot be emitted because the queue is full.
        let (_next2, recorder2) = FakeNext::new();
        let ctx2 = make_context("client", "e2", None);
        let forwarded = gate
            .handle_inner(
                tools_call_extra("2", "echo", serde_json::json!({"i": 2})),
                &ctx2,
                boxed_next(recorder2.clone()),
            )
            .await;
        assert!(!forwarded);

        // Only the first event is in the queue.
        assert!(gate.try_recv().is_some());
        assert!(gate.try_recv().is_none());
    }

    #[tokio::test]
    async fn concurrent_settle_yields_one_forward() {
        let transport = Arc::new(FakeTransport::new());
        let gate = Arc::new(
            PaymentGate::new(
                test_config(PaymentLifecyclePolicy::Transparent),
                transport.clone(),
            )
            .unwrap(),
        );
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);

        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();
        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc...",
            "bitcoin-lightning-bolt11",
            10,
            None,
        )
        .await
        .unwrap();

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let g = gate.clone();
            set.spawn(async move { g.mark_settled("lnbc...", None).await });
        }
        let mut ok_count = 0;
        while let Some(res) = set.join_next().await {
            if res.unwrap().is_ok() {
                ok_count += 1;
            }
        }
        assert_eq!(ok_count, 1);

        // Only one forwarded request.
        assert!(recorder.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn capability_validation_rejects_bad_config() {
        let mut config = test_config(PaymentLifecyclePolicy::Transparent);
        config.priced_capabilities = vec![PricedCapability {
            method: "tools/call".into(),
            name: "echo".into(),
            amount_sats: -1,
            max_amount_sats: None,
            currency_unit: "sats".into(),
            description: "".into(),
        }];
        assert!(PaymentGate::new(config, Arc::new(FakeTransport::new())).is_err());
    }

    #[test]
    fn parse_priced_capabilities_json_works() {
        let json = r#"[{"method":"tools/call","name":"echo","amount":1000,"currencyUnit":"sats"}]"#;
        let caps = PaymentGate::parse_priced_capabilities_json(json).unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].amount_sats, 1000);
    }

    #[tokio::test]
    async fn gating_settle_then_retry_requires_new_payment() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Gating),
            transport.clone(),
        )
        .unwrap();
        let ctx = make_context("client", "e1", Some(PaymentInteractionMode::ExplicitGating));
        let (_next, _recorder) = FakeNext::new();

        let forwarded = gate
            .handle_inner(tools_call("1", "echo"), &ctx, boxed_next(_recorder.clone()))
            .await;
        assert!(!forwarded);
        let event = gate.try_recv().expect("event emitted");

        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc...",
            "bitcoin-lightning-bolt11",
            10,
            None,
        )
        .await
        .unwrap();

        gate.mark_settled("lnbc...", None).await.unwrap();

        // Client retries with a new request. It should be forwarded once using the
        // grant, and the local state must be removed so a second retry needs a new
        // invoice.
        let ctx2 = make_context("client", "e2", Some(PaymentInteractionMode::ExplicitGating));
        let (_next2, recorder2) = FakeNext::new();
        let forwarded = gate
            .handle_inner(
                tools_call("2", "echo"),
                &ctx2,
                boxed_next(recorder2.clone()),
            )
            .await;
        assert!(forwarded);

        // A second retry is a fresh payment event, not a reusable free authorization.
        let (_next3, _recorder3) = FakeNext::new();
        let ctx3 = make_context("client", "e3", Some(PaymentInteractionMode::ExplicitGating));
        let forwarded = gate
            .handle_inner(
                tools_call("3", "echo"),
                &ctx3,
                boxed_next(_recorder3.clone()),
            )
            .await;
        assert!(!forwarded);
        assert!(gate.try_recv().is_some());
    }

    #[tokio::test]
    async fn gating_mark_replayed_forwards_current_request() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Gating),
            transport.clone(),
        )
        .unwrap();
        let ctx = make_context("client", "e1", Some(PaymentInteractionMode::ExplicitGating));
        let (_next, recorder) = FakeNext::new();

        let forwarded = gate
            .handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        assert!(!forwarded);
        let event = gate.try_recv().expect("event emitted");

        // Foreign consumer already has a cached result; forward the current request.
        gate.mark_replayed(&event.request_event_id).await.unwrap();

        assert_eq!(
            recorder.lock().unwrap().as_ref().map(|m| match m {
                JsonRpcMessage::Request(r) => r.method.clone(),
                _ => String::new(),
            }),
            Some("tools/call".into())
        );

        // A subsequent identical request is a new payment event, not a free replay.
        let (_next2, _recorder2) = FakeNext::new();
        let ctx2 = make_context("client", "e2", Some(PaymentInteractionMode::ExplicitGating));
        let forwarded = gate
            .handle_inner(
                tools_call("2", "echo"),
                &ctx2,
                boxed_next(_recorder2.clone()),
            )
            .await;
        assert!(!forwarded);
        assert!(gate.try_recv().is_some());
    }

    #[tokio::test]
    async fn payment_required_error_byte_identical_to_sdk() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Gating),
            transport.clone(),
        )
        .unwrap();
        let ctx = make_context("client", "e1", Some(PaymentInteractionMode::ExplicitGating));
        let (_next, _recorder) = FakeNext::new();

        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(_recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc123",
            "bitcoin-lightning-bolt11",
            10,
            Some("test"),
        )
        .await
        .unwrap();

        let response =
            find_response(&transport.records()).expect("targeted payment_required error");

        let option = PaymentOption {
            amount: 1000,
            pmi: "bitcoin-lightning-bolt11".into(),
            pay_req: "lnbc123".into(),
            description: Some("test".into()),
            ttl: Some(10),
            meta: None,
        };
        let expected = build_payment_required_error(serde_json::json!("1"), option);

        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            serde_json::to_value(&expected).unwrap(),
        );
    }

    #[tokio::test(start_paused = true)]
    async fn payment_pending_error_byte_identical_to_sdk() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Gating),
            transport.clone(),
        )
        .unwrap();
        let ctx = make_context("client", "e1", Some(PaymentInteractionMode::ExplicitGating));
        let (_next, _recorder) = FakeNext::new();

        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(_recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        // A duplicate before an invoice is issued answers with -32043.
        let ctx2 = make_context("client", "e2", Some(PaymentInteractionMode::ExplicitGating));
        let (_next2, _recorder2) = FakeNext::new();
        gate.handle_inner(
            tools_call("2", "echo"),
            &ctx2,
            boxed_next(_recorder2.clone()),
        )
        .await;

        let response = find_response(&transport.records()).expect("payment_pending error");
        let remaining_ms = 10_000; // park_ttl is 10 s in the paused clock
        let expected = build_payment_pending_error(serde_json::json!("2"), remaining_ms);

        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            serde_json::to_value(&expected).unwrap(),
        );

        // The original request is still parked and can be invoiced.
        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc123",
            "bitcoin-lightning-bolt11",
            10,
            Some("test"),
        )
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn slow_publish_success_survives_concurrent_rejection_transparent() {
        let transport = Arc::new(FakeTransport::new());
        transport.set_notification_delay_ms(5_000);
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Transparent),
            transport.clone(),
        )
        .unwrap();
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e-slow-t", None);
        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        // A: slow publish (5s on the paused clock).
        let gate_a = gate.clone();
        let id_a = event.request_event_id.clone();
        let a = tokio::spawn(async move {
            gate_a
                .submit_invoice(
                    &id_a,
                    1000,
                    "lnbc-slow-t",
                    "bitcoin-lightning-bolt11",
                    10,
                    None,
                )
                .await
        });
        // Let A enter Publishing and block inside its slow send.
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;

        // B: concurrent submit of the same invoice must be rejected while A publishes.
        let b_res = gate
            .submit_invoice(
                &event.request_event_id,
                1000,
                "lnbc-slow-t",
                "bitcoin-lightning-bolt11",
                10,
                None,
            )
            .await;
        assert!(
            b_res.is_err(),
            "concurrent submit while publishing must be rejected"
        );
        assert!(format!("{b_res:?}").contains("in flight"));

        // A completes successfully — its success must NOT be lost.
        let a_res = a.await.unwrap();
        assert!(a_res.is_ok(), "slow publish success was lost: {a_res:?}");

        // Settlement resolves against A's registered pay_req and forwards once.
        gate.mark_settled("lnbc-slow-t", None).await.unwrap();
        assert!(
            recorder.lock().unwrap().is_some(),
            "original request was forwarded after settle"
        );
        let required = transport
            .records()
            .iter()
            .filter(|r| matches!(r, TransportRecord::Notification { notification: JsonRpcMessage::Notification(n), .. } if n.method == PAYMENT_REQUIRED_METHOD))
            .count();
        assert_eq!(required, 1, "exactly one payment_required notification");
    }

    #[tokio::test(start_paused = true)]
    async fn slow_publish_success_survives_concurrent_rejection_gating() {
        let transport = Arc::new(FakeTransport::new());
        transport.set_notification_delay_ms(5_000);
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Gating),
            transport.clone(),
        )
        .unwrap();
        let (_next, _recorder) = FakeNext::new();
        let ctx = make_context(
            "client",
            "e-slow-g",
            Some(PaymentInteractionMode::ExplicitGating),
        );
        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(_recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        let gate_a = gate.clone();
        let id_a = event.request_event_id.clone();
        let a = tokio::spawn(async move {
            gate_a
                .submit_invoice(
                    &id_a,
                    1000,
                    "lnbc-slow-g",
                    "bitcoin-lightning-bolt11",
                    10,
                    None,
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;

        let b_res = gate
            .submit_invoice(
                &event.request_event_id,
                1000,
                "lnbc-slow-g",
                "bitcoin-lightning-bolt11",
                10,
                None,
            )
            .await;
        assert!(
            b_res.is_err(),
            "concurrent submit while publishing must be rejected"
        );
        assert!(
            format!("{b_res:?}").contains("in flight"),
            "gating duplicate must hit the Publishing coalescing arm, got {b_res:?}"
        );

        let a_res = a.await.unwrap();
        assert!(a_res.is_ok(), "slow publish success was lost: {a_res:?}");

        // The -32042 targeted response was sent exactly once, and settlement
        // registers the grant for the retrying client.
        gate.mark_settled("lnbc-slow-g", None).await.unwrap();
        let responses = transport
            .records()
            .iter()
            .filter(|r| matches!(r, TransportRecord::Response { .. }))
            .count();
        assert_eq!(responses, 1, "exactly one targeted -32042 response");
    }

    #[tokio::test]
    async fn transparent_submit_invoice_failed_publish_is_retryable() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Transparent),
            transport.clone(),
        )
        .unwrap();
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);

        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        transport.fail_next_notification(1);
        let err = gate
            .submit_invoice(
                &event.request_event_id,
                1000,
                "lnbc...",
                "bitcoin-lightning-bolt11",
                10,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Payment);

        // No payment_required notification was recorded (the publish failed).
        assert!(find_notification(&transport.records(), PAYMENT_REQUIRED_METHOD).is_none());

        // The state is back to AwaitingInvoice, so the same invoice can be retried.
        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc...",
            "bitcoin-lightning-bolt11",
            10,
            None,
        )
        .await
        .unwrap();

        assert!(find_notification(&transport.records(), PAYMENT_REQUIRED_METHOD).is_some());

        // Settlement still forwards the original parked Next.
        gate.mark_settled("lnbc...", None).await.unwrap();
        assert!(recorder.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn gating_submit_invoice_failed_publish_is_retryable() {
        let transport = Arc::new(FakeTransport::new());
        let gate = PaymentGate::new(
            test_config(PaymentLifecyclePolicy::Gating),
            transport.clone(),
        )
        .unwrap();
        let (_next, _recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", Some(PaymentInteractionMode::ExplicitGating));

        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(_recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        transport.fail_next_response(1);
        let err = gate
            .submit_invoice(
                &event.request_event_id,
                1000,
                "lnbc...",
                "bitcoin-lightning-bolt11",
                10,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Payment);

        // No targeted -32042 was recorded (the publish failed).
        assert!(find_response(&transport.records()).is_none());

        // The state is back to AwaitingInvoice, so the same invoice can be retried.
        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc...",
            "bitcoin-lightning-bolt11",
            10,
            None,
        )
        .await
        .unwrap();

        assert!(find_response(&transport.records()).is_some());
    }

    #[tokio::test]
    async fn concurrent_settle_with_duplicate_requests_yields_one_forward() {
        let transport = Arc::new(FakeTransport::new());
        let gate = Arc::new(
            PaymentGate::new(
                test_config(PaymentLifecyclePolicy::Transparent),
                transport.clone(),
            )
            .unwrap(),
        );
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e1", None);

        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        gate.submit_invoice(
            &event.request_event_id,
            1000,
            "lnbc-concurrent",
            "bitcoin-lightning-bolt11",
            10,
            None,
        )
        .await
        .unwrap();

        const N: usize = 10;
        let barrier = Arc::new(tokio::sync::Barrier::new(N + 1));

        enum RaceResult {
            Duplicate {
                forwarded: bool,
                recorder: Arc<std::sync::Mutex<Option<JsonRpcMessage>>>,
            },
            Settled(std::result::Result<(), FfiError>),
        }

        let mut set = tokio::task::JoinSet::new();
        for i in 0..N {
            let g = gate.clone();
            let b = barrier.clone();
            let (_dup_next, dup_recorder) = FakeNext::new();
            let ctx_dup = make_context("client", &format!("e-dup-{i}"), None);
            set.spawn(async move {
                b.wait().await;
                let forwarded = g
                    .handle_inner(
                        tools_call(&format!("dup-{i}"), "echo"),
                        &ctx_dup,
                        boxed_next(dup_recorder.clone()),
                    )
                    .await;
                RaceResult::Duplicate {
                    forwarded,
                    recorder: dup_recorder,
                }
            });
        }

        let gate_for_settle = gate.clone();
        let b = barrier.clone();
        set.spawn(async move {
            b.wait().await;
            RaceResult::Settled(gate_for_settle.mark_settled("lnbc-concurrent", None).await)
        });

        let mut ok_settles = 0;
        let mut forwards = 0;
        while let Some(res) = set.join_next().await {
            match res.unwrap() {
                RaceResult::Duplicate {
                    forwarded,
                    recorder,
                } => {
                    if forwarded {
                        forwards += 1;
                    }
                    assert!(
                        recorder.lock().unwrap().is_none(),
                        "duplicate must not forward"
                    );
                }
                RaceResult::Settled(r) => {
                    if r.is_ok() {
                        ok_settles += 1;
                    }
                }
            }
        }

        assert_eq!(ok_settles, 1, "exactly one mark_settled wins");
        assert_eq!(forwards, 0, "duplicates never forward");
        assert!(
            recorder.lock().unwrap().is_some(),
            "the original parked Next was forwarded exactly once"
        );
    }

    #[tokio::test]
    async fn concurrent_submit_invoice_rejected_attempt_cannot_clobber_invoice_transparent() {
        let transport = Arc::new(FakeTransport::new());
        let gate = Arc::new(
            PaymentGate::new(
                test_config(PaymentLifecyclePolicy::Transparent),
                transport.clone(),
            )
            .unwrap(),
        );
        let (_next, recorder) = FakeNext::new();
        let ctx = make_context("client", "e-concurrent-t", None);
        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        // Use a barrier so both submit_invoice calls start together. They race
        // on the same Publishing state; the winner is the one that still owns
        // the nonce by the time it sends, the other is rejected and must not
        // clobber the issued invoice.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let request_event_id = event.request_event_id.clone();
        let gate_a = gate.clone();
        let gate_b = gate.clone();
        let b = barrier.clone();

        let request_event_id_a = request_event_id.clone();
        let a = tokio::spawn(async move {
            b.wait().await;
            gate_a
                .submit_invoice(
                    &request_event_id_a,
                    1000,
                    "lnbc-concurrent-t",
                    "bitcoin-lightning-bolt11",
                    10,
                    None,
                )
                .await
        });
        let b = tokio::spawn(async move {
            barrier.wait().await;
            gate_b
                .submit_invoice(
                    &request_event_id,
                    1000,
                    "lnbc-concurrent-t",
                    "bitcoin-lightning-bolt11",
                    10,
                    None,
                )
                .await
        });

        let (a_res, b_res) = tokio::join!(a, b);
        let a_res = a_res.unwrap();
        let b_res = b_res.unwrap();
        assert!(
            (a_res.is_ok() && b_res.is_err()) || (a_res.is_err() && b_res.is_ok()),
            "exactly one concurrent submit may succeed; got {a_res:?} and {b_res:?}"
        );

        // Settlement must still work; the rejected attempt did not clobber the
        // successful issue.
        gate.mark_settled("lnbc-concurrent-t", None).await.unwrap();
        assert!(
            recorder.lock().unwrap().is_some(),
            "original request was forwarded exactly once"
        );

        let records = transport.records();
        let required_count = records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    TransportRecord::Notification {
                        notification: JsonRpcMessage::Notification(n),
                        ..
                    } if n.method == PAYMENT_REQUIRED_METHOD
                )
            })
            .count();
        assert_eq!(required_count, 1, "only one payment_required may be sent");
    }

    #[tokio::test]
    async fn concurrent_submit_invoice_rejected_attempt_cannot_clobber_invoice_gating() {
        let transport = Arc::new(FakeTransport::new());
        let gate = Arc::new(
            PaymentGate::new(
                test_config(PaymentLifecyclePolicy::Gating),
                transport.clone(),
            )
            .unwrap(),
        );
        let (_next, _recorder) = FakeNext::new();
        let ctx = make_context(
            "client",
            "e-concurrent-g",
            Some(PaymentInteractionMode::ExplicitGating),
        );
        gate.handle_inner(tools_call("1", "echo"), &ctx, boxed_next(_recorder.clone()))
            .await;
        let event = gate.try_recv().unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let request_event_id = event.request_event_id.clone();
        let gate_a = gate.clone();
        let gate_b = gate.clone();
        let b = barrier.clone();

        let request_event_id_a = request_event_id.clone();
        let a = tokio::spawn(async move {
            b.wait().await;
            gate_a
                .submit_invoice(
                    &request_event_id_a,
                    1000,
                    "lnbc-concurrent-g",
                    "bitcoin-lightning-bolt11",
                    10,
                    None,
                )
                .await
        });
        let b = tokio::spawn(async move {
            barrier.wait().await;
            gate_b
                .submit_invoice(
                    &request_event_id,
                    1000,
                    "lnbc-concurrent-g",
                    "bitcoin-lightning-bolt11",
                    10,
                    None,
                )
                .await
        });

        let (a_res, b_res) = tokio::join!(a, b);
        let a_res = a_res.unwrap();
        let b_res = b_res.unwrap();
        assert!(
            (a_res.is_ok() && b_res.is_err()) || (a_res.is_err() && b_res.is_ok()),
            "exactly one concurrent submit may succeed; got {a_res:?} and {b_res:?}"
        );

        // Settlement stores a grant.
        gate.mark_settled("lnbc-concurrent-g", None).await.unwrap();

        // A retry with the same canonical identity but a new request_event_id
        // must forward the original request.
        let (_next2, recorder2) = FakeNext::new();
        let ctx2 = make_context(
            "client",
            "e-retry",
            Some(PaymentInteractionMode::ExplicitGating),
        );
        gate.handle_inner(
            tools_call("2", "echo"),
            &ctx2,
            boxed_next(recorder2.clone()),
        )
        .await;
        assert!(
            recorder2.lock().unwrap().is_some(),
            "paid grant must forward the request"
        );

        let records = transport.records();
        let response_count = records
            .iter()
            .filter(|r| matches!(r, TransportRecord::Response { .. }))
            .count();
        assert_eq!(response_count, 1, "only one targeted -32042 may be sent");
    }
}
