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

// The module is not yet wired into the broader FFI surface (W1c will consume it
// in a later phase), so public types and helpers are intentionally "dead" for
// now.  This is a temporary Phase-2 allowance.
#![allow(dead_code)]

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
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    PaymentInteractionMode,
};
use contextvm_sdk::payments::authorization_store::{AuthorizationStore, ClaimOrPending};
use contextvm_sdk::payments::canonical::{
    compute_canonical_invocation_identity, CanonicalInvocationIdentity,
};
use contextvm_sdk::payments::constants::{
    PAYMENT_ACCEPTED_METHOD, PAYMENT_PENDING_ERROR_CODE, PAYMENT_REJECTED_METHOD,
    PAYMENT_REQUIRED_ERROR_CODE, PAYMENT_REQUIRED_METHOD, PMI_BITCOIN_LIGHTNING_BOLT11,
};
use contextvm_sdk::payments::types::{
    Meta, PaymentAcceptedParams, PaymentOption, PaymentPendingErrorData, PaymentRejectedParams,
    PaymentRequiredErrorData, PaymentRequiredParams,
};
use contextvm_sdk::transport::server::{InboundContext, InboundMiddleware, Next};

use crate::error::{ErrorCode, FfiError};

/// Extra headroom on top of the submitted invoice TTL + execution budget.
///
/// The gate enforces `ttl + execution_budget + margin < min(request_timeout, session_timeout)`
/// so a payment round-trip fits inside the route's overall timeout.
const ROUTE_BUDGET_MARGIN_SECS: u64 = 5;

/// Instructions embedded in payment-required wire errors.
const PAYMENT_REQUIRED_INSTRUCTIONS: &str = "Payment is required for this capability.";

/// Instructions embedded in payment-pending wire errors.
const PAYMENT_PENDING_INSTRUCTIONS: &str =
    "Payment is already pending; retry after the supplied interval.";

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
#[derive(Debug, Clone)]
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
    /// Consume the continuation and forward `message` down the chain.
    fn run(self: Box<Self>, message: JsonRpcMessage) -> Pin<Box<dyn Future<Output = bool> + Send>>;
}

struct SdkNext(Next);

impl PaymentNext for SdkNext {
    fn run(self: Box<Self>, message: JsonRpcMessage) -> Pin<Box<dyn Future<Output = bool> + Send>> {
        let next = (*self).0;
        Box::pin(async move { next.run(message).await })
    }
}

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
    next_and_message: Option<(Box<dyn PaymentNext>, JsonRpcMessage)>,
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
        }
    }
}

#[derive(Debug, Clone)]
enum PaymentState {
    AwaitingInvoice,
    InvoiceIssued {
        pay_req: String,
        amount: i64,
        pmi: String,
        ttl_secs: u64,
        description: Option<String>,
    },
    Granted,
    Claiming,
    Replayed,
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
        let (canonical_key, nonce, expires_at, lifecycle) = self.prepare_invoice(
            request_event_id,
            amount_sats,
            pay_req,
            pmi,
            ttl_secs,
            description,
        )?;

        if lifecycle == PaymentLifecyclePolicy::Gating {
            self.send_payment_required_error(&canonical_key).await;
        } else {
            self.send_payment_required_notification(&canonical_key)
                .await;
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
        let outcome = self.prepare_settle(pay_req)?;

        match outcome {
            SettleOutcome::Transparent {
                canonical_key,
                identity,
                next_and_message,
                client_pubkey,
                request_event_id,
                mirrored_wrap_kind,
                amount,
                pmi,
                ttl_ms,
            } => {
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

                let forwarded = if let Some((next, message)) = next_and_message {
                    next.run(message).await
                } else {
                    false
                };

                self.inner.auth_store.grant(&identity, ttl_ms);
                self.set_state_replayed(
                    &canonical_key,
                    tokio::time::Instant::now() + self.park_ttl(),
                );
                if forwarded {
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

        let (entry, lifecycle) = {
            let mut parking = self.inner.parking.lock();
            let entry = parking
                .remove(&canonical_key)
                .ok_or_else(|| payment_error("parked entry disappeared"))?;
            self.inner.auth_store.clear_pending(&entry.identity);
            let lifecycle = self.lifecycle_for(&entry);
            (entry, lifecycle)
        };

        if lifecycle == PaymentLifecyclePolicy::Transparent {
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
    /// In transparent mode the parked `Next` continuation is forwarded
    /// immediately and free.  In gating mode there is no parked Next to forward,
    /// so the canonical identity is marked `Replayed`; the next retry will be
    /// forwarded free without ever emitting a `-32042`.
    pub async fn mark_replayed(&self, request_event_id: &str) -> Result<(), FfiError> {
        let outcome = self.prepare_replay(request_event_id)?;

        self.inner
            .auth_store
            .grant(&outcome.identity, self.park_ttl_ms());
        if outcome.lifecycle == PaymentLifecyclePolicy::Transparent {
            if let Some((next, message)) = outcome.next_and_message {
                next.run(message).await;
            }
        }
        Ok(())
    }

    /// Validate a JSON string containing a list of priced capabilities.
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
        let snapshot = {
            let parking = self.inner.parking.lock();
            parking.by_key.get(&canonical_key).and_then(|entry| {
                if entry.expires_at > now {
                    Some(ParkedEntrySnapshot::from(entry))
                } else {
                    None
                }
            })
        };
        if let Some(snapshot) = snapshot {
            return self
                .handle_live_entry(&canonical_key, snapshot, ctx, &request, lifecycle, next)
                .await;
        }

        // No live local entry.  Consult the authorization store first, which
        // also lets crash-seeded grants win without re-parking.
        match self
            .inner
            .auth_store
            .claim_or_set_pending(&identity, self.park_ttl_ms())
        {
            ClaimOrPending::Claimed => {
                let result = next.run(JsonRpcMessage::Request(request.clone())).await;
                let expires = now + self.park_ttl();
                self.set_state_replayed(&canonical_key, expires);
                result
            }
            ClaimOrPending::AlreadyPending { remaining_ms } => {
                if lifecycle == PaymentLifecyclePolicy::Gating {
                    self.send_payment_pending(ctx, &request.id, remaining_ms)
                        .await;
                }
                false
            }
            ClaimOrPending::PendingSet => {
                let params_json = match request.params.as_ref() {
                    Some(p) => serde_json::to_string(p).unwrap_or_default(),
                    None => String::new(),
                };

                // Ensure the parked registry has capacity before locking.
                self.make_room().await;

                let mut parking = self.inner.parking.lock();

                let nonce = self.inner.nonce.fetch_add(1, Ordering::SeqCst);
                let expires_at = now + self.park_ttl();
                let capability_name = self
                    .capability_name_for_request(&request)
                    .unwrap_or_else(|| capability.name.clone());

                let next_and_message = if lifecycle == PaymentLifecyclePolicy::Transparent {
                    Some((next, JsonRpcMessage::Request(request.clone())))
                } else {
                    None
                };

                let entry = ParkedEntry {
                    identity: identity.clone(),
                    request_event_id: ctx.request_event_id.clone(),
                    client_pubkey: ctx.client_pubkey.clone(),
                    mirrored_wrap_kind: ctx.mirrored_wrap_kind,
                    capability,
                    request,
                    next_and_message,
                    state: PaymentState::AwaitingInvoice,
                    expires_at,
                    nonce,
                    lifecycle,
                };

                let event = PaymentGateRequest {
                    request_event_id: ctx.request_event_id.clone(),
                    client_pubkey: ctx.client_pubkey.clone(),
                    method: "tools/call".into(),
                    params_json,
                    capability_name,
                    canonical_invocation_id: canonical_key.clone(),
                };

                parking.insert(&canonical_key, entry);
                drop(parking);

                if self.inner.events_tx.try_send(event).is_err() {
                    // Queue overflow: roll back the park and clear pending.
                    self.remove_and_clear(&canonical_key);
                    return false;
                }

                tokio::spawn(self.clone().ttl_worker(canonical_key, nonce, expires_at));
                false
            }
            _ => false,
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
            PaymentState::AwaitingInvoice => {
                let remaining = self.remaining_ms(snapshot.expires_at, now);
                if lifecycle == PaymentLifecyclePolicy::Gating {
                    self.send_payment_pending(ctx, &request.id, remaining).await;
                }
                false
            }
            PaymentState::InvoiceIssued {
                pay_req,
                amount,
                pmi,
                ttl_secs,
                description,
            } => {
                if lifecycle == PaymentLifecyclePolicy::Gating {
                    let remaining = self.remaining_ms(snapshot.expires_at, now);
                    self.send_payment_pending(ctx, &request.id, remaining).await;
                } else {
                    self.send_payment_required_notification_data(
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
                false
            }
            PaymentState::Granted => {
                // Serialise concurrent claim attempts via a transient `Claiming` state.
                if !self.set_claiming(canonical_key) {
                    let remaining = self.park_ttl_ms();
                    if lifecycle == PaymentLifecyclePolicy::Gating {
                        self.send_payment_pending(ctx, &request.id, remaining).await;
                    }
                    return false;
                }

                if self.inner.auth_store.claim(&snapshot.identity) {
                    let result = next.run(JsonRpcMessage::Request(request.clone())).await;
                    let expires = now + self.park_ttl();
                    self.set_state_replayed(canonical_key, expires);
                    result
                } else {
                    // Grant was concurrently consumed or expired.  Treat as new
                    // pending rather than silently failing.
                    self.remove_and_clear(canonical_key);
                    if lifecycle == PaymentLifecyclePolicy::Gating {
                        let remaining = self.park_ttl_ms();
                        self.send_payment_pending(ctx, &request.id, remaining).await;
                    }
                    false
                }
            }
            PaymentState::Claiming => {
                let remaining = self.remaining_ms(snapshot.expires_at, now);
                if lifecycle == PaymentLifecyclePolicy::Gating {
                    self.send_payment_pending(ctx, &request.id, remaining).await;
                }
                false
            }
            PaymentState::Replayed => {
                // Refresh TTL before forwarding the *current* request.
                {
                    let mut parking = self.inner.parking.lock();
                    if let Some(e) = parking.by_key.get_mut(canonical_key) {
                        if matches!(e.state, PaymentState::Replayed) {
                            e.expires_at = now + self.park_ttl();
                        } else {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                next.run(JsonRpcMessage::Request(request.clone())).await
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

    fn lifecycle_for(&self, entry: &ParkedEntry) -> PaymentLifecyclePolicy {
        entry.lifecycle
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

    async fn send_payment_required_error(&self, canonical_key: &str) {
        let (client_pubkey, request_event_id, request_id, amount, pay_req, pmi, ttl, description) = {
            let parking = self.inner.parking.lock();
            let Some(entry) = parking.by_key.get(canonical_key) else {
                return;
            };
            let PaymentState::InvoiceIssued {
                pay_req,
                amount,
                pmi,
                ttl_secs,
                description,
            } = &entry.state
            else {
                return;
            };
            (
                entry.client_pubkey.clone(),
                entry.request_event_id.clone(),
                entry.request.id.clone(),
                *amount,
                pay_req.clone(),
                pmi.clone(),
                *ttl_secs,
                description.clone(),
            )
        };

        let option = PaymentOption {
            amount,
            pmi,
            pay_req,
            description,
            ttl: Some(ttl),
            meta: None,
        };
        let data = PaymentRequiredErrorData {
            instructions: Some(PAYMENT_REQUIRED_INSTRUCTIONS.into()),
            payment_options: vec![option],
        };
        let response = JsonRpcErrorResponse {
            jsonrpc: "2.0".into(),
            id: request_id,
            error: JsonRpcError {
                code: PAYMENT_REQUIRED_ERROR_CODE,
                message: "Payment Required".into(),
                data: Some(serde_json::to_value(&data).expect("serialize")),
            },
        };
        let _ = self
            .inner
            .transport
            .send_targeted_response(
                client_pubkey,
                request_event_id,
                JsonRpcMessage::ErrorResponse(response),
            )
            .await;
    }

    async fn send_payment_required_notification(&self, canonical_key: &str) {
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
                return;
            };
            let PaymentState::InvoiceIssued {
                pay_req,
                amount,
                pmi,
                ttl_secs,
                description,
            } = &entry.state
            else {
                return;
            };
            (
                entry.client_pubkey.clone(),
                entry.request_event_id.clone(),
                entry.mirrored_wrap_kind,
                *amount,
                pay_req.clone(),
                pmi.clone(),
                *ttl_secs,
                description.clone(),
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
        .await;
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
    ) {
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

    async fn send_payment_pending(
        &self,
        ctx: &InboundContext,
        request_id: &Value,
        remaining_ms: u64,
    ) {
        let retry_after = (remaining_ms.div_ceil(1000)).clamp(1, 2);
        let data = PaymentPendingErrorData {
            instructions: Some(PAYMENT_PENDING_INSTRUCTIONS.into()),
            retry_after: Some(retry_after),
        };
        let response = JsonRpcErrorResponse {
            jsonrpc: "2.0".into(),
            id: request_id.clone(),
            error: JsonRpcError {
                code: PAYMENT_PENDING_ERROR_CODE,
                message: "Payment Pending".into(),
                data: Some(serde_json::to_value(&data).expect("serialize")),
            },
        };
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

    async fn make_room(&self) {
        loop {
            let evicted = {
                let mut parking = self.inner.parking.lock();
                if parking.by_key.len() < self.inner.config.parked_cap {
                    return;
                }
                parking.evict_oldest()
            };
            if let Some((_, entry)) = evicted {
                self.inner.auth_store.clear_pending(&entry.identity);
                if entry.lifecycle == PaymentLifecyclePolicy::Transparent {
                    let (pmi, amount) = payment_rejection_details(&entry);
                    let message = "parked capacity exceeded";
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
            }
        }
    }

    fn remove_and_clear(&self, canonical_key: &str) {
        let mut parking = self.inner.parking.lock();
        if let Some(entry) = parking.remove(canonical_key) {
            self.inner.auth_store.clear_pending(&entry.identity);
        }
    }

    fn set_state_replayed(&self, canonical_key: &str, expires_at: tokio::time::Instant) {
        let mut parking = self.inner.parking.lock();
        let Some(entry) = parking.by_key.get_mut(canonical_key) else {
            return;
        };
        let old = std::mem::replace(&mut entry.state, PaymentState::Replayed);
        let pay_req_to_remove = if let PaymentState::InvoiceIssued { pay_req, .. } = old {
            Some(pay_req)
        } else {
            None
        };
        let request_event_id = entry.request_event_id.clone();
        entry.next_and_message = None;
        entry.expires_at = expires_at;
        let _ = entry;
        if let Some(pay_req) = pay_req_to_remove {
            parking.by_payreq.remove(&pay_req);
        }
        parking.by_event.remove(&request_event_id);
    }

    async fn ttl_worker(self, key: String, nonce: u64, expires_at: tokio::time::Instant) {
        tokio::time::sleep_until(expires_at).await;
        self.expire(&key, nonce).await;
    }

    async fn expire(&self, key: &str, nonce: u64) {
        let now = tokio::time::Instant::now();
        let entry = {
            let mut parking = self.inner.parking.lock();
            let Some(e) = parking.by_key.get(key) else {
                return;
            };
            if e.nonce != nonce || e.expires_at > now {
                return;
            }
            parking.remove(key).expect("entry")
        };

        let lifecycle = self.lifecycle_for(&entry);
        self.inner.auth_store.clear_pending(&entry.identity);

        if lifecycle == PaymentLifecyclePolicy::Transparent {
            let (pmi, amount) = payment_rejection_details(&entry);
            let message = "payment window expired";
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

impl PaymentState {
    fn amount(&self) -> Option<i64> {
        match self {
            PaymentState::InvoiceIssued { amount, .. } => Some(*amount),
            _ => None,
        }
    }
}

/// Outcome of preparing a `mark_settled` call.
#[allow(clippy::large_enum_variant)]
enum SettleOutcome {
    Transparent {
        canonical_key: String,
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

/// Outcome of preparing a `mark_replayed` call.
struct ReplayOutcome {
    canonical_key: String,
    identity: CanonicalInvocationIdentity,
    next_and_message: Option<(Box<dyn PaymentNext>, JsonRpcMessage)>,
    lifecycle: PaymentLifecyclePolicy,
}

impl PaymentGate {
    fn prepare_invoice(
        &self,
        request_event_id: &str,
        amount_sats: i64,
        pay_req: &str,
        pmi: &str,
        ttl_secs: u64,
        description: Option<&str>,
    ) -> Result<(String, u64, tokio::time::Instant, PaymentLifecyclePolicy), FfiError> {
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
        let entry = parking
            .by_key
            .get_mut(&canonical_key)
            .ok_or_else(|| payment_error("parked entry disappeared"))?;

        // Validate amount against the advertised capability.
        if amount_sats < entry.capability.amount_sats {
            return Err(validation_error("amount below capability minimum"));
        }
        if let Some(max) = entry.capability.max_amount_sats {
            if amount_sats > max {
                return Err(validation_error("amount above capability maximum"));
            }
        }

        // Validate PMI and TTL.
        if pmi != PMI_BITCOIN_LIGHTNING_BOLT11 {
            return Err(validation_error("unsupported payment method identifier"));
        }
        if ttl_secs == 0
            || ttl_secs > self.inner.config.payment_ttl_cap_secs
            || ttl_secs > self.max_invoice_ttl_secs()
        {
            return Err(validation_error("ttl violates payment or route budget cap"));
        }

        match &entry.state {
            PaymentState::AwaitingInvoice => {
                let nonce = self.inner.nonce.fetch_add(1, Ordering::SeqCst);
                let expires_at = now + Duration::from_secs(ttl_secs);
                entry.state = PaymentState::InvoiceIssued {
                    pay_req: pay_req.to_string(),
                    amount: amount_sats,
                    pmi: pmi.to_string(),
                    ttl_secs,
                    description: description.map(String::from),
                };
                entry.nonce = nonce;
                entry.expires_at = expires_at;
                let identity = entry.identity.clone();
                let lifecycle = entry.lifecycle;
                let _ = entry;

                self.inner
                    .auth_store
                    .update_pending_ttl(&identity, ttl_secs * 1000);
                parking
                    .by_payreq
                    .insert(pay_req.to_string(), canonical_key.clone());
                drop(parking);
                Ok((canonical_key, nonce, expires_at, lifecycle))
            }
            PaymentState::InvoiceIssued {
                pay_req: existing, ..
            } => {
                if existing != pay_req {
                    return Err(validation_error("double-invoice guard: pay_req mismatch"));
                }
                // Re-bind with the same pay_req: refresh TTL, amount must match.
                if let Some(amount) = entry.state.amount() {
                    if amount != amount_sats {
                        return Err(validation_error("re-bind amount mismatch"));
                    }
                }
                let nonce = self.inner.nonce.fetch_add(1, Ordering::SeqCst);
                let expires_at = now + Duration::from_secs(ttl_secs);
                entry.nonce = nonce;
                entry.expires_at = expires_at;
                if let PaymentState::InvoiceIssued {
                    description: ref mut d,
                    ..
                } = entry.state
                {
                    *d = description.map(String::from);
                }
                let identity = entry.identity.clone();
                let lifecycle = entry.lifecycle;
                let _ = entry;

                self.inner
                    .auth_store
                    .update_pending_ttl(&identity, ttl_secs * 1000);
                drop(parking);
                Ok((canonical_key, nonce, expires_at, lifecycle))
            }
            _ => Err(payment_error("payment already settled or replayed")),
        }
    }

    fn prepare_settle(&self, pay_req: &str) -> Result<SettleOutcome, FfiError> {
        let now = tokio::time::Instant::now();
        let canonical_key = {
            let parking = self.inner.parking.lock();
            parking
                .by_payreq
                .get(pay_req)
                .cloned()
                .ok_or_else(|| payment_error("unknown pay_req"))?
        };

        let mut parking = self.inner.parking.lock();
        let entry = parking
            .by_key
            .get_mut(&canonical_key)
            .ok_or_else(|| payment_error("parked entry disappeared"))?;

        // Inspect a clone of the state so the mutable entry can be modified
        // without violating borrow rules.
        match entry.state.clone() {
            PaymentState::InvoiceIssued {
                pay_req,
                amount,
                pmi,
                ttl_secs,
                ..
            } => {
                let identity = entry.identity.clone();
                let ttl_ms = ttl_secs * 1000;
                let lifecycle = entry.lifecycle;

                if lifecycle == PaymentLifecyclePolicy::Transparent {
                    // Take the parked continuation, remove the pay_req mapping, and
                    // mark the entry as claiming while the wire work is in flight.
                    let next_and_message = entry.next_and_message.take();
                    let client_pubkey = entry.client_pubkey.clone();
                    let request_event_id = entry.request_event_id.clone();
                    let mirrored_wrap_kind = entry.mirrored_wrap_kind;

                    entry.state = PaymentState::Claiming;
                    entry.expires_at = now + self.park_ttl();
                    parking.by_payreq.remove(&pay_req);
                    drop(parking);

                    Ok(SettleOutcome::Transparent {
                        canonical_key,
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
                    // Gating: store the grant and stay parked (no Next).
                    let nonce = self.inner.nonce.fetch_add(1, Ordering::SeqCst);
                    let expires_at = now + Duration::from_secs(ttl_secs);
                    entry.state = PaymentState::Granted;
                    entry.nonce = nonce;
                    entry.expires_at = expires_at;
                    entry.next_and_message = None;
                    self.inner.auth_store.grant(&identity, ttl_ms);
                    drop(parking);

                    Ok(SettleOutcome::Gating {
                        canonical_key,
                        nonce,
                        expires_at,
                    })
                }
            }
            _ => Err(payment_error("no outstanding invoice for pay_req")),
        }
    }

    fn prepare_replay(&self, request_event_id: &str) -> Result<ReplayOutcome, FfiError> {
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
        let entry = parking
            .by_key
            .get_mut(&canonical_key)
            .ok_or_else(|| payment_error("parked entry disappeared"))?;

        // Inspect a clone of the state; if it can be replayed, replace it and
        // clean up the lookup maps while still holding the lock.
        match entry.state.clone() {
            PaymentState::AwaitingInvoice | PaymentState::InvoiceIssued { .. } => {
                let identity = entry.identity.clone();
                let lifecycle = entry.lifecycle;
                let next_and_message = entry.next_and_message.take();
                let old = std::mem::replace(&mut entry.state, PaymentState::Replayed);
                let pay_req_to_remove = if let PaymentState::InvoiceIssued { pay_req, .. } = old {
                    Some(pay_req)
                } else {
                    None
                };
                let event_id = entry.request_event_id.clone();
                entry.next_and_message = None;
                entry.expires_at = now + self.park_ttl();
                if let Some(pay_req) = pay_req_to_remove {
                    parking.by_payreq.remove(&pay_req);
                }
                parking.by_event.remove(&event_id);
                drop(parking);

                Ok(ReplayOutcome {
                    canonical_key,
                    identity,
                    next_and_message,
                    lifecycle,
                })
            }
            _ => Err(payment_error("request already settled or replayed")),
        }
    }
}

fn canonical_key_for(identity: &CanonicalInvocationIdentity) -> String {
    format!("{}:{}", identity.client_pubkey, identity.invocation_hash)
}

fn payment_rejection_details(entry: &ParkedEntry) -> (&str, i64) {
    match &entry.state {
        PaymentState::InvoiceIssued { pmi, amount, .. } => (pmi.as_str(), *amount),
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
    use contextvm_sdk::core::types::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest};
    use contextvm_sdk::payments::constants::{
        PAYMENT_ACCEPTED_METHOD, PAYMENT_REJECTED_METHOD, PAYMENT_REQUIRED_METHOD,
    };
    use contextvm_sdk::transport::server::InboundContext;
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
    struct FakeTransport(Arc<std::sync::Mutex<Vec<TransportRecord>>>);

    impl FakeTransport {
        fn new() -> Self {
            Self(Arc::new(std::sync::Mutex::new(Vec::new())))
        }

        fn records(&self) -> Vec<TransportRecord> {
            self.0.lock().unwrap().clone()
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
            let inner = self.0.clone();
            Box::pin(async move {
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
            let inner = self.0.clone();
            Box::pin(async move {
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
        fn run(
            self: Box<Self>,
            message: JsonRpcMessage,
        ) -> Pin<Box<dyn Future<Output = bool> + Send>> {
            Box::pin(async move {
                *self.0.lock().unwrap() = Some(message);
                true
            })
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

        // Identical call now replays without a new invoice.
        let (_next3, recorder3) = FakeNext::new();
        let ctx2 = make_context("client", "e2", None);
        let forwarded = gate
            .handle_inner(
                tools_call("3", "echo"),
                &ctx2,
                boxed_next(recorder3.clone()),
            )
            .await;
        assert!(forwarded);
        assert!(gate.try_recv().is_none());
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
    async fn transparent_mark_replayed_free_forward() {
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

        // Foreign consumer already has a cached result; forward free.
        gate.mark_replayed(&event.request_event_id).await.unwrap();

        let (_next2, recorder2) = FakeNext::new();
        let ctx2 = make_context("client", "e2", None);
        let forwarded = gate
            .handle_inner(
                tools_call("2", "echo"),
                &ctx2,
                boxed_next(recorder2.clone()),
            )
            .await;
        assert!(forwarded);
        assert!(gate.try_recv().is_none());
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
}
