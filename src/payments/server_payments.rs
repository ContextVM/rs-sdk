//! CEP-8 transparent-lifecycle server payment middleware.
//!
//! A priced invocation is gated at the transport seam: the middleware emits
//! `notifications/payment_required`, waits for the configured [`PaymentProcessor`]
//! to verify settlement, emits `notifications/payment_accepted`, and only then
//! forwards the request to the MCP handler. A dynamic pricing callback can
//! reject the invocation (emitting `notifications/payment_rejected` and
//! dropping it) or waive payment (forwarding it untouched). Duplicate
//! deliveries of one request event share one payment and are never charged
//! twice within the configured bounds.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt, Shared};
use lru::LruCache;
use tokio::time::Instant;

use crate::core::types::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, PaymentInteractionMode,
};
use crate::payments::constants::{
    DEFAULT_MAX_PENDING_PAYMENTS, DEFAULT_PAYMENT_TTL_MS, PAYMENT_ACCEPTED_METHOD,
    PAYMENT_REJECTED_METHOD, PAYMENT_REQUIRED_METHOD,
};
use crate::payments::errors::PaymentError;
use crate::payments::server_payments_utils::{
    build_processors_by_pmi, match_priced_capability, resolve_and_initiate_payment,
    InitiationOutcome,
};
use crate::payments::traits::{PaymentProcessor, ResolvePrice};
use crate::payments::types::{
    PaymentAcceptedParams, PaymentInteractionPolicy, PaymentProcessorVerifyParams,
    PaymentRejectedParams, PaymentRequiredParams, PricedCapability,
};
use crate::transport::server::middleware::{InboundContext, InboundMiddleware, Next};
use crate::transport::server::PaymentNotificationSender;

const LOG_TARGET: &str = "contextvm_sdk::payments::server_payments";

/// How many entries the opportunistic pending-cache purge inspects per lookup, from the
/// least-recently-used end. Internal hygiene knob with no wire meaning; the correctness
/// gate is the expiry check at lookup, not this scan.
const PENDING_PURGE_SCAN_LIMIT: usize = 25;

/// Configuration for the server-side payment middlewares.
///
/// Mirrors the reference implementation's options object: the same six fields,
/// with the millisecond TTL expressed as a [`Duration`].
#[derive(Clone)]
#[non_exhaustive]
pub struct ServerPaymentsOptions {
    /// Payment processors, one per PMI. The first entry is the fallback when the
    /// client advertises no PMI the server has a processor for.
    pub processors: Vec<Arc<dyn PaymentProcessor>>,
    /// The capability patterns that are priced. First match wins; an entry with
    /// `name: None` prices every invocation of its method.
    pub priced_capabilities: Vec<PricedCapability>,
    /// Optional dynamic pricing callback computing a per-request quote (or
    /// rejecting / waiving). `None` charges each capability's static amount.
    pub resolve_price: Option<Arc<dyn ResolvePrice>>,
    /// How long a request stays in pending-payment state.
    ///
    /// This bounds the redelivery dedup: a duplicate delivery of the same
    /// request event inside this window shares the first delivery's payment
    /// and is never re-charged; outside it (or after a process restart, or
    /// under pending-cache capacity pressure) a redelivery re-runs the
    /// lifecycle. The settlement wait itself is bounded by the minimum of this
    /// value and the verification timeout derived from the payment request's
    /// own `ttl`.
    ///
    /// Setting this above the transport's session timeout (300 s by default)
    /// is a silently paid-but-undelivered configuration: the paying client's
    /// session can expire before its payment resolves, costing the client the
    /// acceptance notification (the paid result itself is still delivered
    /// from the captured route snapshot).
    pub payment_ttl: Duration,
    /// Maximum number of concurrently tracked pending-payment request ids (a
    /// DoS/memory guardrail). When the cache is full and every entry is live,
    /// a new priced request is refused rather than evicting a live payment.
    pub max_pending_payments: usize,
    /// Which payment-interaction lifecycles the server accepts. Carried here so
    /// the options stay API-stable; consumed by the payments configuration
    /// entry point, not by the transparent middleware itself.
    pub payment_interaction: PaymentInteractionPolicy,
}

impl ServerPaymentsOptions {
    /// Build options with the given processors and priced capabilities, and the
    /// reference defaults for everything else: no dynamic pricing, a 300 s
    /// payment TTL, 1000 tracked pending payments, and the permissive
    /// [`PaymentInteractionPolicy::Optional`].
    pub fn new(
        processors: Vec<Arc<dyn PaymentProcessor>>,
        priced_capabilities: Vec<PricedCapability>,
    ) -> Self {
        Self {
            processors,
            priced_capabilities,
            resolve_price: None,
            payment_ttl: Duration::from_millis(DEFAULT_PAYMENT_TTL_MS),
            max_pending_payments: DEFAULT_MAX_PENDING_PAYMENTS,
            payment_interaction: PaymentInteractionPolicy::default(),
        }
    }

    /// Set the dynamic pricing callback.
    pub fn with_resolve_price(mut self, resolve_price: Arc<dyn ResolvePrice>) -> Self {
        self.resolve_price = Some(resolve_price);
        self
    }

    /// Set how long a request stays in pending-payment state.
    pub fn with_payment_ttl(mut self, payment_ttl: Duration) -> Self {
        self.payment_ttl = payment_ttl;
        self
    }

    /// Set the cap on concurrently tracked pending-payment request ids.
    pub fn with_max_pending_payments(mut self, max_pending_payments: usize) -> Self {
        self.max_pending_payments = max_pending_payments;
        self
    }

    /// Set the accepted payment-interaction policy.
    pub fn with_payment_interaction(mut self, policy: PaymentInteractionPolicy) -> Self {
        self.payment_interaction = policy;
        self
    }
}

/// Inputs to [`create_server_payments_middleware`].
///
/// A struct rather than a positional list so later payment work can add fields (a shared
/// processor map is already optional here) without a breaking signature change.
#[non_exhaustive]
pub struct ServerPaymentsMiddlewareParams {
    /// The payment configuration (processors, priced capabilities, TTLs).
    pub options: ServerPaymentsOptions,
    /// The transport's injected payment-notification publish
    /// ([`NostrServerTransport::payment_notification_sender`](crate::transport::server::NostrServerTransport::payment_notification_sender)).
    pub sender: PaymentNotificationSender,
    /// Pre-built PMI-to-processor map, shared across middlewares. Built locally when `None`.
    pub processors_by_pmi: Option<Arc<HashMap<String, Arc<dyn PaymentProcessor>>>>,
}

impl ServerPaymentsMiddlewareParams {
    /// Build params from the two required inputs, with no shared processor map.
    pub fn new(options: ServerPaymentsOptions, sender: PaymentNotificationSender) -> Self {
        Self {
            options,
            sender,
            processors_by_pmi: None,
        }
    }
}

/// Create the CEP-8 transparent-lifecycle payment middleware.
///
/// Three self-gates run before any payment work, each forwarding the message untouched:
/// a non-request is never gated; a session whose negotiated payment-interaction mode is
/// present and not transparent is another lifecycle's business; and an unpriced request
/// is free. Everything else runs the lifecycle: emit `notifications/payment_required`,
/// verify settlement through the configured processor, emit
/// `notifications/payment_accepted`, and forward.
///
/// The middleware chain runs on a detached task, so long verification never blocks the
/// transport's inbound loop. Cancellation rides the per-event token on
/// [`InboundContext`]: it is a child of the transport's shutdown token, so closing the
/// transport aborts an in-flight verification instead of leaving it running against
/// cleared state. Only the verification phase is bounded that way: the initiation phase
/// (the pricing callback plus the processor's payment-request creation) runs under no
/// timeout and does not observe cancellation, matching the reference implementation, so
/// a processor that hangs in creation parks its chain task past shutdown.
pub fn create_server_payments_middleware(
    params: ServerPaymentsMiddlewareParams,
) -> Arc<dyn InboundMiddleware> {
    let processors_by_pmi = params
        .processors_by_pmi
        .unwrap_or_else(|| Arc::new(build_processors_by_pmi(&params.options.processors)));
    let capacity = NonZeroUsize::new(params.options.max_pending_payments)
        .unwrap_or_else(|| NonZeroUsize::new(1).expect("1 is non-zero"));
    Arc::new(ServerPaymentsMiddleware {
        options: params.options,
        sender: params.sender,
        processors_by_pmi,
        pending: tokio::sync::Mutex::new(LruCache::new(capacity)),
    })
}

/// How one transparent lifecycle run ended, from the dedup's point of view.
///
/// Private: the retention rule below is the only consumer.
enum LifecycleFailure {
    /// Price resolution or `create_payment_required` failed: no invoice exists.
    Initiate(PaymentError),
    /// The `payment_required` publish failed: treated as no invoice reaching the client.
    PublishRequired(crate::Error),
    /// The `payment_rejected` publish failed (also before any invoice).
    PublishRejected(crate::Error),
    /// `verify_payment` returned an error: the invoice exists and may have settled.
    Verify(PaymentError),
    /// The verification deadline elapsed: the invoice exists and may settle later.
    VerifyTimeout,
}

impl LifecycleFailure {
    /// Whether this failure happened strictly before an invoice could have reached the
    /// client. The retention rule: delete the pending entry only for pre-invoice
    /// failures, because once an invoice exists the client's money may already be gone
    /// and a redelivery must never mint a second charge.
    fn is_pre_invoice(&self) -> bool {
        match self {
            Self::Initiate(_) | Self::PublishRequired(_) | Self::PublishRejected(_) => true,
            Self::Verify(_) | Self::VerifyTimeout => false,
        }
    }
}

impl std::fmt::Display for LifecycleFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initiate(e) => write!(f, "payment initiation failed: {e}"),
            Self::PublishRequired(e) => write!(f, "payment_required publish failed: {e}"),
            Self::PublishRejected(e) => write!(f, "payment_rejected publish failed: {e}"),
            Self::Verify(e) => write!(f, "payment verification failed: {e}"),
            Self::VerifyTimeout => write!(f, "payment verification timed out"),
        }
    }
}

type InFlight = Shared<BoxFuture<'static, Arc<Result<(), LifecycleFailure>>>>;

/// One tracked pending payment: when the dedup window closes, and the shared lifecycle
/// future a duplicate delivery joins.
struct PendingEntry {
    expires_at: Instant,
    in_flight: InFlight,
}

/// Opportunistic hygiene against one-shot spam: drop up to
/// [`PENDING_PURGE_SCAN_LIMIT`] expired entries per lookup, scanning from the
/// least-recently-used end (`iter()` yields most-recently-used first, so the scan needs
/// `.rev()` to inspect the oldest entries, which are the ones that can be expired).
fn purge_expired_pending(cache: &mut LruCache<String, PendingEntry>, now: Instant) {
    let expired: Vec<String> = cache
        .iter()
        .rev()
        .take(PENDING_PURGE_SCAN_LIMIT)
        .filter(|(_, entry)| entry.expires_at <= now)
        .map(|(key, _)| key.clone())
        .collect();
    for key in expired {
        cache.pop(&key);
    }
}

/// The three wire notification builders. Serializing the params structs cannot fail for
/// the shapes this middleware produces, but the signature is honest about serde.
fn payment_notification(
    method: &str,
    params: impl serde::Serialize,
) -> Result<JsonRpcMessage, PaymentError> {
    Ok(JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: method.to_string(),
        params: Some(serde_json::to_value(params)?),
    }))
}

struct ServerPaymentsMiddleware {
    options: ServerPaymentsOptions,
    sender: PaymentNotificationSender,
    processors_by_pmi: Arc<HashMap<String, Arc<dyn PaymentProcessor>>>,
    /// The pending-payment dedup cache. A `tokio::sync::Mutex`, but the guard is NEVER
    /// held across an await: the single critical section below covers purge, lookup and
    /// insert only, and every await happens after it drops. Holding it across the verify
    /// await would serialize every priced request in the server behind one payment.
    pending: tokio::sync::Mutex<LruCache<String, PendingEntry>>,
}

#[async_trait]
impl InboundMiddleware for ServerPaymentsMiddleware {
    async fn handle(&self, message: JsonRpcMessage, ctx: &InboundContext, next: Next) -> bool {
        // Self-gate 1: only requests are gated; notifications and responses pass.
        let request = match &message {
            JsonRpcMessage::Request(request) => request.clone(),
            _ => return next.run(message).await,
        };

        // Self-gate 2: a session negotiated onto another lifecycle is not this
        // middleware's business. An absent mode does NOT forward: it falls through to
        // the priced check, mirroring the reference implementation (in production the
        // seam always resolves the mode, so `None` is reachable only from a hand-built
        // context).
        if let Some(mode) = ctx.payment_interaction {
            if mode != PaymentInteractionMode::Transparent {
                return next.run(message).await;
            }
        }

        // Self-gate 3: an unpriced request is free.
        let Some(priced) =
            match_priced_capability(&request, &self.options.priced_capabilities).cloned()
        else {
            return next.run(message).await;
        };

        tracing::debug!(
            target: LOG_TARGET,
            method = %request.method,
            request_event_id = %ctx.request_event_id,
            client_pubkey = %ctx.client_pubkey,
            "priced capability matched"
        );

        let event_id = ctx.request_event_id.clone();
        let now = Instant::now();

        // Built before the guard is taken, so no allocation happens inside the critical
        // section. An async block is inert until polled, so nothing has run yet.
        let fresh: InFlight = run_lifecycle(
            self.options.clone(),
            Arc::clone(&self.processors_by_pmi),
            Arc::clone(&self.sender),
            request,
            message,
            priced,
            ctx.clone(),
            next,
        )
        .boxed()
        .shared();

        // ONE critical section covering purge, lookup and insert. This is the rule that
        // prevents a double charge under concurrent duplicate delivery; splitting the
        // lookup and the insert into separate lock acquisitions is the defect. The
        // guard drops before any await.
        let existing = {
            let mut cache = self.pending.lock().await;
            purge_expired_pending(&mut cache, now);

            match cache.peek(&event_id) {
                Some(entry) if entry.expires_at > now => Some(entry.in_flight.clone()),
                _ => {
                    // Fail closed: never let `put`'s recency eviction remove a live
                    // entry. `LruCache::put` evicts strictly by recency, so on a full
                    // cache an expired entry mid-order would SURVIVE while the live
                    // entry at the LRU end dies, disarming its dedup. Pop a specific
                    // expired key first; refuse only when every entry is live.
                    if cache.len() == cache.cap().get() {
                        let expired_key = cache
                            .iter()
                            .find(|(_, entry)| entry.expires_at <= now)
                            .map(|(key, _)| key.clone());
                        match expired_key {
                            Some(key) => {
                                cache.pop(&key);
                            }
                            None => {
                                tracing::warn!(
                                    target: LOG_TARGET,
                                    event_id = %event_id,
                                    "pending-payment capacity reached with every entry \
                                     live; refusing to start a new payment"
                                );
                                return false; // drop, do not forward
                            }
                        }
                    }
                    cache.put(
                        event_id.clone(),
                        PendingEntry {
                            expires_at: now + self.options.payment_ttl,
                            in_flight: fresh.clone(),
                        },
                    );
                    None
                }
            }
        }; // guard dropped BEFORE any await

        if let Some(in_flight) = existing {
            // A duplicate delivery joins the in-flight (or completed) lifecycle and
            // never forwards; the first delivery's future owns the forward.
            let _ = in_flight.await;
            return false;
        }

        let result = fresh.clone().await;

        // Delete ONLY on a strictly pre-invoice failure, and only if this task's own
        // entry is still the one at the key: a same-key entry inserted by a later
        // legitimate retry must never be popped by a stale cleanup.
        if matches!(result.as_ref(), Err(e) if e.is_pre_invoice()) {
            let mut cache = self.pending.lock().await;
            if cache
                .peek(&event_id)
                .is_some_and(|entry| entry.in_flight.ptr_eq(&fresh))
            {
                cache.pop(&event_id);
            }
        }

        matches!(result.as_ref(), Ok(()))
    }
}

/// The whole transparent lifecycle for one priced request, owned by the shared future so
/// exactly one forward can happen per request event id (only the first delivery's `Next`
/// is moved in; `Next` is not `Clone`).
///
/// A panic anywhere inside is caught and converted to a failure classified by whether an
/// invoice had been issued, so a panicking processor neither propagates into a duplicate
/// awaiter nor leaves the entry undeletable.
#[allow(clippy::too_many_arguments)]
async fn run_lifecycle(
    options: ServerPaymentsOptions,
    processors_by_pmi: Arc<HashMap<String, Arc<dyn PaymentProcessor>>>,
    sender: PaymentNotificationSender,
    request: JsonRpcRequest,
    message: JsonRpcMessage,
    priced: PricedCapability,
    ctx: InboundContext,
    next: Next,
) -> Arc<Result<(), LifecycleFailure>> {
    let invoice_issued = Arc::new(AtomicBool::new(false));
    let issued_flag = Arc::clone(&invoice_issued);
    let event_id = ctx.request_event_id.clone();

    let inner = run_lifecycle_inner(
        options,
        processors_by_pmi,
        sender,
        request,
        message,
        priced,
        ctx,
        next,
        issued_flag,
    );
    match AssertUnwindSafe(inner).catch_unwind().await {
        Ok(result) => {
            if let Err(failure) = &result {
                tracing::warn!(
                    target: LOG_TARGET,
                    event_id = %event_id,
                    failure = %failure,
                    "payment lifecycle did not complete"
                );
            }
            Arc::new(result)
        }
        Err(_panic) => {
            tracing::error!(
                target: LOG_TARGET,
                event_id = %event_id,
                "payment lifecycle panicked"
            );
            Arc::new(Err(if invoice_issued.load(Ordering::SeqCst) {
                // Post-invoice: keep the entry until TTL, exactly like a verify error;
                // the client may already have paid.
                LifecycleFailure::Verify(PaymentError::Processor(
                    "payment lifecycle panicked after the invoice was issued".to_string(),
                ))
            } else {
                LifecycleFailure::Initiate(PaymentError::Processor(
                    "payment lifecycle panicked before any invoice was issued".to_string(),
                ))
            }))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_lifecycle_inner(
    options: ServerPaymentsOptions,
    processors_by_pmi: Arc<HashMap<String, Arc<dyn PaymentProcessor>>>,
    sender: PaymentNotificationSender,
    request: JsonRpcRequest,
    message: JsonRpcMessage,
    priced: PricedCapability,
    ctx: InboundContext,
    next: Next,
    invoice_issued: Arc<AtomicBool>,
) -> Result<(), LifecycleFailure> {
    let event_id = ctx.request_event_id.clone();
    let client_pubkey = ctx.client_pubkey.clone();

    let outcome = resolve_and_initiate_payment(
        &request,
        &priced,
        &event_id,
        &client_pubkey,
        ctx.client_pmis.as_deref(),
        &options,
        &processors_by_pmi,
    )
    .await
    .map_err(LifecycleFailure::Initiate)?;

    let (processor, payment_required, merged_meta, verify_timeout) = match outcome {
        InitiationOutcome::Rejected {
            pmi,
            amount,
            message: reason,
        } => {
            tracing::info!(
                target: LOG_TARGET,
                request_event_id = %event_id,
                pmi = %pmi,
                amount,
                reason = reason.as_deref().unwrap_or(""),
                "payment rejected"
            );
            let rejected = payment_notification(
                PAYMENT_REJECTED_METHOD,
                PaymentRejectedParams {
                    pmi,
                    amount: Some(amount),
                    message: reason,
                },
            )
            .map_err(LifecycleFailure::Initiate)?;
            sender(
                client_pubkey.clone(),
                event_id.clone(),
                ctx.mirrored_wrap_kind,
                rejected,
            )
            .await
            .map_err(LifecycleFailure::PublishRejected)?;
            // Dropped: the request is answered by the rejection, not forwarded.
            return Ok(());
        }
        InitiationOutcome::Waived => {
            tracing::debug!(
                target: LOG_TARGET,
                request_event_id = %event_id,
                "payment waived, forwarding priced request"
            );
            next.run(message).await;
            return Ok(());
        }
        InitiationOutcome::PaymentRequired {
            processor,
            payment_required,
            merged_meta,
            verify_timeout,
        } => (processor, payment_required, merged_meta, verify_timeout),
    };

    let required = payment_notification(
        PAYMENT_REQUIRED_METHOD,
        PaymentRequiredParams {
            amount: payment_required.amount,
            pay_req: payment_required.pay_req.clone(),
            pmi: payment_required.pmi.clone(),
            description: payment_required.description.clone(),
            ttl: payment_required.ttl,
            meta: merged_meta,
        },
    )
    .map_err(LifecycleFailure::Initiate)?;

    tracing::info!(
        target: LOG_TARGET,
        request_event_id = %event_id,
        pmi = %payment_required.pmi,
        amount = payment_required.amount,
        ttl = payment_required.ttl,
        "payment required notification sent"
    );

    sender(
        client_pubkey.clone(),
        event_id.clone(),
        ctx.mirrored_wrap_kind,
        required,
    )
    .await
    .map_err(LifecycleFailure::PublishRequired)?;
    // From this point the client can be holding a payable invoice, so no failure below
    // may delete the pending entry.
    invoice_issued.store(true, Ordering::SeqCst);

    // The settlement wait is bounded by the stricter of the invoice's own TTL and the
    // configured pending TTL; those are deliberately different clocks (the entry's
    // `expires_at` above uses the pending TTL alone, which bounds redelivery dedup).
    let polling_timeout = verify_timeout.min(options.payment_ttl);
    let verify_cancel = ctx.cancel.child_token();

    tracing::debug!(
        target: LOG_TARGET,
        request_event_id = %event_id,
        pmi = %payment_required.pmi,
        timeout_ms = polling_timeout.as_millis() as u64,
        "verifying payment"
    );

    let verify_result = tokio::time::timeout(
        polling_timeout,
        processor.verify_payment(PaymentProcessorVerifyParams {
            pay_req: payment_required.pay_req.clone(),
            request_event_id: event_id.clone(),
            client_pubkey: client_pubkey.clone(),
            cancel: verify_cancel.clone(),
        }),
    )
    .await;
    // Not dead code: `timeout` drops the verify future, but a processor that spawned
    // its own background poller keeps ticking until this cancel. Fired on the
    // completion path too, mirroring an abort-on-settle contract.
    verify_cancel.cancel();

    let verified = match verify_result {
        Err(_elapsed) => return Err(LifecycleFailure::VerifyTimeout),
        Ok(Err(error)) => return Err(LifecycleFailure::Verify(error)),
        Ok(Ok(verified)) => verified,
    };

    tracing::info!(
        target: LOG_TARGET,
        request_event_id = %event_id,
        pmi = %payment_required.pmi,
        amount = payment_required.amount,
        "payment accepted"
    );

    let accepted = payment_notification(
        PAYMENT_ACCEPTED_METHOD,
        PaymentAcceptedParams {
            amount: payment_required.amount,
            pmi: payment_required.pmi.clone(),
            meta: verified.meta,
        },
    );
    // A verified payment forwards unconditionally. The money has moved: delivering the
    // paid-for result is the invocation's whole point, while the acceptance
    // notification is a courtesy the spec only SHOULDs. A publish failure here (the
    // paying client's session can be gone after a long payment) is logged and the
    // forward still happens. Do not "tidy" this back into an early return.
    match accepted {
        Ok(accepted) => {
            if let Err(error) = sender(
                client_pubkey.clone(),
                event_id.clone(),
                ctx.mirrored_wrap_kind,
                accepted,
            )
            .await
            {
                tracing::error!(
                    target: LOG_TARGET,
                    request_event_id = %event_id,
                    error = %error,
                    "payment verified but the acceptance notification failed to \
                     publish; forwarding the paid request anyway"
                );
            }
        }
        Err(error) => {
            tracing::error!(
                target: LOG_TARGET,
                request_event_id = %event_id,
                error = %error,
                "payment verified but the acceptance notification failed to build; \
                 forwarding the paid request anyway"
            );
        }
    }

    tracing::debug!(
        target: LOG_TARGET,
        request_event_id = %event_id,
        "forwarding priced request after payment"
    );
    next.run(message).await;
    Ok(())
}

// `Arc<dyn PaymentProcessor>` / `Arc<dyn ResolvePrice>` are not `Debug`, so the
// derive is unavailable; print the processor PMIs and the resolver's presence.
impl std::fmt::Debug for ServerPaymentsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerPaymentsOptions")
            .field(
                "processors",
                &self.processors.iter().map(|p| p.pmi()).collect::<Vec<_>>(),
            )
            .field("priced_capabilities", &self.priced_capabilities)
            .field(
                "resolve_price",
                &self.resolve_price.as_ref().map(|_| "Some(..)"),
            )
            .field("payment_ttl", &self.payment_ttl)
            .field("max_pending_payments", &self.max_pending_payments)
            .field("payment_interaction", &self.payment_interaction)
            .finish()
    }
}

#[cfg(test)]
mod option_tests {
    use super::*;

    #[test]
    fn defaults_match_ts_sdk() {
        let options = ServerPaymentsOptions::new(Vec::new(), Vec::new());
        assert_eq!(options.payment_ttl, Duration::from_millis(300_000));
        assert_eq!(options.max_pending_payments, 1000);
        assert!(options.resolve_price.is_none());
        assert_eq!(
            options.payment_interaction,
            PaymentInteractionPolicy::Optional
        );
    }

    #[test]
    fn builders_override_each_default() {
        struct NeverResolve;
        #[async_trait::async_trait]
        impl ResolvePrice for NeverResolve {
            async fn resolve_price(
                &self,
                _params: crate::payments::types::ResolvePriceParams,
            ) -> Result<
                crate::payments::types::ResolvePriceResult,
                crate::payments::errors::PaymentError,
            > {
                unreachable!("never called in this test")
            }
        }

        let options = ServerPaymentsOptions::new(Vec::new(), Vec::new())
            .with_resolve_price(Arc::new(NeverResolve))
            .with_payment_ttl(Duration::from_secs(7))
            .with_max_pending_payments(3)
            .with_payment_interaction(PaymentInteractionPolicy::Transparent);
        assert!(options.resolve_price.is_some());
        assert_eq!(options.payment_ttl, Duration::from_secs(7));
        assert_eq!(options.max_pending_payments, 3);
        assert_eq!(
            options.payment_interaction,
            PaymentInteractionPolicy::Transparent
        );
    }

    #[test]
    fn debug_prints_pmis_without_processor_internals() {
        let options = ServerPaymentsOptions::new(Vec::new(), Vec::new());
        let printed = format!("{options:?}");
        assert!(printed.contains("ServerPaymentsOptions"));
        assert!(printed.contains("max_pending_payments: 1000"));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::core::types::JsonRpcRequest;
    use crate::payments::traits::ResolvePrice;
    use crate::payments::types::{
        PaymentProcessorCreateParams, ResolvePriceParams, ResolvePriceResult, VerifyOutcome,
    };
    use crate::transport::open_stream::OpenStreamConfig;
    use crate::transport::server::middleware::run_inbound_chain;
    use crate::transport::server::{IncomingRequest, ServerEventRouteStore, ServerOpenStreamState};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
    use tokio_util::sync::CancellationToken;

    // ── local doubles (this module runs in every feature configuration) ──

    /// Ordered record of lifecycle steps: "create", "verify", "publish:<method>", "forward".
    #[derive(Clone, Default)]
    struct EventLog(Arc<StdMutex<Vec<String>>>);

    impl EventLog {
        fn push(&self, item: impl Into<String>) {
            self.0.lock().unwrap().push(item.into());
        }
        fn items(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
        fn count_of(&self, item: &str) -> usize {
            self.items().iter().filter(|i| i.as_str() == item).count()
        }
    }

    /// Published notifications: (method, params).
    #[derive(Clone, Default)]
    struct Sent(Arc<StdMutex<Vec<(String, serde_json::Value)>>>);

    impl Sent {
        fn items(&self) -> Vec<(String, serde_json::Value)> {
            self.0.lock().unwrap().clone()
        }
        fn methods(&self) -> Vec<String> {
            self.items().into_iter().map(|(m, _)| m).collect()
        }
    }

    /// A recording sender double; methods in `fail_methods` return `Err` without recording.
    fn test_sender(
        log: EventLog,
        sent: Sent,
        fail_methods: &'static [&'static str],
    ) -> PaymentNotificationSender {
        Arc::new(move |_pk, _event_id, _wrap, notification| {
            let log = log.clone();
            let sent = sent.clone();
            Box::pin(async move {
                if let JsonRpcMessage::Notification(n) = &notification {
                    log.push(format!("publish:{}", n.method));
                    if fail_methods.contains(&n.method.as_str()) {
                        return Err(crate::Error::Other("publish failed".to_string()));
                    }
                    sent.0.lock().unwrap().push((
                        n.method.clone(),
                        n.params.clone().unwrap_or(serde_json::Value::Null),
                    ));
                }
                Ok(())
            })
        })
    }

    enum CreatePlan {
        Ok,
        OkAfter(Duration),
        Err,
        ErrAfter(Duration),
        Panic,
    }

    enum VerifyPlan {
        /// Settle immediately.
        Instant,
        /// Settle after a delay.
        SettleAfter(Duration),
        /// Never settle. Spawns a watcher (like a background poller) that records the
        /// elapsed time at which the per-verify cancellation fires.
        Hang,
    }

    struct TestProcessor {
        pmi: String,
        ttl: Option<u64>,
        log: EventLog,
        create_calls: Arc<AtomicUsize>,
        /// Per-call behaviors, popped front; an empty queue means `Ok`.
        create_plan: StdMutex<VecDeque<CreatePlan>>,
        verify_plan: VerifyPlan,
        cancel_observed_after: Arc<StdMutex<Option<Duration>>>,
    }

    impl TestProcessor {
        fn new(pmi: &str, log: EventLog) -> Self {
            Self {
                pmi: pmi.to_string(),
                ttl: None,
                log,
                create_calls: Arc::new(AtomicUsize::new(0)),
                create_plan: StdMutex::new(VecDeque::new()),
                verify_plan: VerifyPlan::Instant,
                cancel_observed_after: Arc::new(StdMutex::new(None)),
            }
        }

        fn with_ttl(mut self, ttl: u64) -> Self {
            self.ttl = Some(ttl);
            self
        }

        fn with_verify(mut self, plan: VerifyPlan) -> Self {
            self.verify_plan = plan;
            self
        }

        fn with_create_plan(self, plan: Vec<CreatePlan>) -> Self {
            *self.create_plan.lock().unwrap() = plan.into();
            self
        }
    }

    #[async_trait]
    impl PaymentProcessor for TestProcessor {
        fn pmi(&self) -> &str {
            &self.pmi
        }

        async fn create_payment_required(
            &self,
            params: PaymentProcessorCreateParams,
        ) -> Result<PaymentRequiredParams, PaymentError> {
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            self.log.push("create");
            let plan = self.create_plan.lock().unwrap().pop_front();
            match plan {
                Some(CreatePlan::Err) => {
                    return Err(PaymentError::Processor("create failed".to_string()))
                }
                Some(CreatePlan::ErrAfter(delay)) => {
                    tokio::time::sleep(delay).await;
                    return Err(PaymentError::Processor("create failed".to_string()));
                }
                Some(CreatePlan::OkAfter(delay)) => tokio::time::sleep(delay).await,
                Some(CreatePlan::Panic) => panic!("processor create panicked"),
                Some(CreatePlan::Ok) | None => {}
            }
            Ok(PaymentRequiredParams {
                amount: params.amount,
                pay_req: format!("invoice-{}", params.request_event_id),
                pmi: self.pmi.clone(),
                description: params.description,
                ttl: self.ttl,
                meta: None,
            })
        }

        async fn verify_payment(
            &self,
            params: PaymentProcessorVerifyParams,
        ) -> Result<VerifyOutcome, PaymentError> {
            self.log.push("verify");
            match &self.verify_plan {
                VerifyPlan::Instant => Ok(VerifyOutcome::default()),
                VerifyPlan::SettleAfter(delay) => {
                    tokio::time::sleep(*delay).await;
                    Ok(VerifyOutcome::default())
                }
                VerifyPlan::Hang => {
                    let started = Instant::now();
                    let observed = Arc::clone(&self.cancel_observed_after);
                    let cancel = params.cancel.clone();
                    // A background poller, exactly the shape the per-verify cancel
                    // exists for: the timeout drops the verify future, so only the
                    // explicit cancel can stop this task.
                    tokio::spawn(async move {
                        cancel.cancelled().await;
                        *observed.lock().unwrap() = Some(started.elapsed());
                    });
                    std::future::pending::<()>().await;
                    unreachable!("pending() never resolves")
                }
            }
        }
    }

    struct StaticResolver(ResolvePriceResult);
    #[async_trait]
    impl ResolvePrice for StaticResolver {
        async fn resolve_price(
            &self,
            _params: ResolvePriceParams,
        ) -> Result<ResolvePriceResult, PaymentError> {
            Ok(self.0.clone())
        }
    }

    /// A second middleware placed after the payments one, so "forward" lands in the same
    /// ordered log as the emissions.
    struct RecordForward(EventLog);
    #[async_trait]
    impl InboundMiddleware for RecordForward {
        async fn handle(&self, message: JsonRpcMessage, _ctx: &InboundContext, next: Next) -> bool {
            self.0.push("forward");
            next.run(message).await
        }
    }

    // ── harness ─────────────────────────────────────────────────────

    struct Harness {
        middleware: Arc<dyn InboundMiddleware>,
        forward_recorder: Arc<RecordForward>,
        routes: ServerEventRouteStore,
        open_stream: ServerOpenStreamState,
        tx: UnboundedSender<IncomingRequest>,
        rx: StdMutex<UnboundedReceiver<IncomingRequest>>,
    }

    impl Harness {
        fn new(middleware: Arc<dyn InboundMiddleware>, log: EventLog) -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            Self {
                middleware,
                forward_recorder: Arc::new(RecordForward(log)),
                routes: ServerEventRouteStore::new(),
                open_stream: ServerOpenStreamState::new(&OpenStreamConfig::default(), 10),
                tx,
                rx: StdMutex::new(rx),
            }
        }

        fn chain(&self) -> Arc<[Arc<dyn InboundMiddleware>]> {
            Arc::from(vec![
                Arc::clone(&self.middleware),
                Arc::clone(&self.forward_recorder) as Arc<dyn InboundMiddleware>,
            ])
        }

        /// Run the real chain to completion for one event.
        async fn dispatch(&self, ctx: Arc<InboundContext>, message: JsonRpcMessage) {
            run_inbound_chain(
                self.chain(),
                ctx,
                self.tx.clone(),
                self.routes.clone(),
                self.open_stream.clone(),
                message,
                None,
            )
            .await;
        }

        /// How many messages reached the worker channel so far.
        fn delivered(&self) -> usize {
            let mut count = 0;
            let mut rx = self.rx.lock().unwrap();
            while rx.try_recv().is_ok() {
                count += 1;
            }
            count
        }
    }

    fn priced_call(amount: i64) -> PricedCapability {
        PricedCapability {
            method: "tools/call".to_string(),
            name: Some("echo".to_string()),
            amount,
            max_amount: None,
            currency_unit: "sats".to_string(),
            description: None,
        }
    }

    fn call_message() -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("client-req-1"),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({ "name": "echo" })),
        })
    }

    fn test_ctx(event_id: &str) -> InboundContext {
        InboundContext {
            client_pubkey: "c".repeat(64),
            request_event_id: event_id.to_string(),
            is_encrypted: false,
            mirrored_wrap_kind: None,
            client_pmis: None,
            payment_interaction: Some(PaymentInteractionMode::Transparent),
            cancel: CancellationToken::new(),
        }
    }

    struct Fixture {
        harness: Harness,
        log: EventLog,
        sent: Sent,
        create_calls: Arc<AtomicUsize>,
    }

    fn fixture_with(
        configure: impl FnOnce(ServerPaymentsOptions) -> ServerPaymentsOptions,
        processor: TestProcessor,
        fail_methods: &'static [&'static str],
    ) -> Fixture {
        let log = processor.log.clone();
        let sent = Sent::default();
        let create_calls = Arc::clone(&processor.create_calls);
        let options = configure(ServerPaymentsOptions::new(
            vec![Arc::new(processor)],
            vec![priced_call(21)],
        ));
        let middleware = create_server_payments_middleware(ServerPaymentsMiddlewareParams::new(
            options,
            test_sender(log.clone(), sent.clone(), fail_methods),
        ));
        Fixture {
            harness: Harness::new(middleware, log.clone()),
            log,
            sent,
            create_calls,
        }
    }

    fn fixture(processor: TestProcessor) -> Fixture {
        fixture_with(|o| o, processor, &[])
    }

    // ── the tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn happy_path_emits_required_then_accepted_then_forwards() {
        let log = EventLog::default();
        let fx = fixture(TestProcessor::new("pmi-a", log.clone()).with_ttl(600));
        let ctx = test_ctx("evt-1");
        let ctx_cancel = ctx.cancel.clone();
        fx.harness.dispatch(Arc::new(ctx), call_message()).await;

        // The full ordered sequence, asserted as an order, not a set.
        assert_eq!(
            fx.log.items(),
            vec![
                "create",
                "publish:notifications/payment_required",
                "verify",
                "publish:notifications/payment_accepted",
                "forward",
            ]
        );
        assert_eq!(fx.harness.delivered(), 1);
        let sent = fx.sent.items();
        assert_eq!(sent[0].1.get("amount").unwrap(), 21);
        assert_eq!(sent[0].1.get("pmi").unwrap(), "pmi-a");
        assert!(sent[0].1.get("pay_req").is_some());
        assert_eq!(sent[1].1.get("amount").unwrap(), 21);
        // The per-event context token must survive a completed verify: the middleware
        // cancels its own per-verify CHILD, never the event's token, which later
        // middlewares on the same event still rely on.
        assert!(
            !ctx_cancel.is_cancelled(),
            "the per-event token must not be cancelled by a completed verify"
        );
    }

    #[tokio::test]
    async fn reject_emits_payment_rejected_and_does_not_forward() {
        // Two processors with distinct PMIs, and the client selects the SECOND, so a
        // selection performed after pricing (which would report the default first
        // processor) is observable.
        let log = EventLog::default();
        let sent = Sent::default();
        let first = TestProcessor::new("pmi-first", log.clone());
        let second = TestProcessor::new("pmi-second", log.clone());
        let options = ServerPaymentsOptions::new(
            vec![Arc::new(first), Arc::new(second)],
            vec![priced_call(55)],
        )
        .with_resolve_price(Arc::new(StaticResolver(ResolvePriceResult::Reject {
            message: Some("not today".to_string()),
        })));
        let middleware = create_server_payments_middleware(ServerPaymentsMiddlewareParams::new(
            options,
            test_sender(log.clone(), sent.clone(), &[]),
        ));
        let harness = Harness::new(middleware, log.clone());

        let mut ctx = test_ctx("evt-reject");
        ctx.client_pmis = Some(vec!["pmi-second".to_string()]);
        harness.dispatch(Arc::new(ctx), call_message()).await;

        let items = sent.items();
        assert_eq!(items.len(), 1, "exactly one notification");
        assert_eq!(items[0].0, "notifications/payment_rejected");
        assert_eq!(items[0].1.get("pmi").unwrap(), "pmi-second");
        assert_eq!(items[0].1.get("amount").unwrap(), 55);
        assert_eq!(items[0].1.get("message").unwrap(), "not today");
        assert_eq!(harness.delivered(), 0, "a rejected request is dropped");
    }

    #[tokio::test]
    async fn a_redelivered_rejected_request_re_emits_nothing() {
        let log = EventLog::default();
        let fx = fixture_with(
            |o| {
                o.with_resolve_price(Arc::new(StaticResolver(ResolvePriceResult::Reject {
                    message: None,
                })))
            },
            TestProcessor::new("pmi-a", log.clone()),
            &[],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-r")), call_message())
            .await;
        assert_eq!(fx.sent.methods(), vec!["notifications/payment_rejected"]);

        fx.harness
            .dispatch(Arc::new(test_ctx("evt-r")), call_message())
            .await;
        assert_eq!(
            fx.sent.methods(),
            vec!["notifications/payment_rejected"],
            "a redelivery inside the TTL re-emits nothing"
        );
        assert_eq!(fx.harness.delivered(), 0);
    }

    #[tokio::test]
    async fn waive_forwards_without_emitting() {
        let log = EventLog::default();
        let fx = fixture_with(
            |o| {
                o.with_resolve_price(Arc::new(StaticResolver(ResolvePriceResult::Waive {
                    meta: None,
                })))
            },
            TestProcessor::new("pmi-a", log.clone()),
            &[],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-w")), call_message())
            .await;
        assert_eq!(fx.sent.items().len(), 0, "a waiver emits nothing");
        assert_eq!(fx.harness.delivered(), 1, "a waived request is forwarded");
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn verify_timeout_neither_forwards_nor_accepts() {
        let log = EventLog::default();
        let fx = fixture(
            TestProcessor::new("pmi-a", log.clone())
                .with_ttl(1)
                .with_verify(VerifyPlan::Hang),
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-t")), call_message())
            .await;
        assert_eq!(fx.sent.methods(), vec!["notifications/payment_required"]);
        assert_eq!(
            fx.harness.delivered(),
            0,
            "a timed-out payment never forwards"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn verify_timeout_keeps_the_entry_so_a_retry_does_not_re_charge() {
        let log = EventLog::default();
        let fx = fixture(
            TestProcessor::new("pmi-a", log.clone())
                .with_ttl(1) // 1 s verify bound, far below the 300 s pending TTL
                .with_verify(VerifyPlan::Hang),
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-k")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 1);

        // The client may have paid an invoice whose settlement landed just after the
        // bound; a redelivery must NOT mint a second invoice.
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-k")), call_message())
            .await;
        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            1,
            "a redelivery after a verify timeout must not re-charge"
        );
    }

    #[tokio::test]
    async fn a_pre_invoice_failure_deletes_the_entry_so_a_retry_re_runs() {
        let log = EventLog::default();
        let fx = fixture(
            TestProcessor::new("pmi-a", log.clone())
                .with_create_plan(vec![CreatePlan::Err, CreatePlan::Ok]),
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-p")), call_message())
            .await;
        assert_eq!(fx.harness.delivered(), 0);
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 1);

        // No invoice was ever issued, so the retry is free and must re-run.
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-p")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fx.harness.delivered(), 1, "the retry completes normally");
    }

    #[tokio::test]
    async fn a_failed_invoice_publish_deletes_the_entry_so_a_retry_re_runs() {
        // The invoice never went out (as far as the server can prove), so no money can
        // have moved and the retry must be free; keeping the entry would black-hole the
        // request for the whole TTL with no invoice out.
        let log = EventLog::default();
        let fx = fixture_with(
            |o| o,
            TestProcessor::new("pmi-a", log.clone()),
            &["notifications/payment_required"],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-pubfail")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fx.harness.delivered(), 0);

        fx.harness
            .dispatch(Arc::new(test_ctx("evt-pubfail")), call_message())
            .await;
        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            2,
            "a retry after a failed invoice publish must re-run the lifecycle"
        );
        assert_eq!(fx.harness.delivered(), 0, "still nothing forwarded");
    }

    #[tokio::test]
    async fn a_failed_rejection_publish_deletes_the_entry_so_a_retry_re_runs() {
        // Same rule on the rejection arm: the refusal never reached the client, no
        // invoice exists, so the retry re-runs (and re-attempts the rejection).
        let log = EventLog::default();
        let fx = fixture_with(
            |o| {
                o.with_resolve_price(Arc::new(StaticResolver(ResolvePriceResult::Reject {
                    message: None,
                })))
            },
            TestProcessor::new("pmi-a", log.clone()),
            &["notifications/payment_rejected"],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-rejfail")), call_message())
            .await;
        assert_eq!(fx.log.count_of("publish:notifications/payment_rejected"), 1);

        fx.harness
            .dispatch(Arc::new(test_ctx("evt-rejfail")), call_message())
            .await;
        assert_eq!(
            fx.log.count_of("publish:notifications/payment_rejected"),
            2,
            "a retry after a failed rejection publish must re-run the lifecycle"
        );
        assert_eq!(fx.harness.delivered(), 0);
    }

    #[tokio::test]
    async fn a_verified_payment_forwards_even_when_the_acceptance_fails_to_publish() {
        let log = EventLog::default();
        let fx = fixture_with(
            |o| o,
            TestProcessor::new("pmi-a", log.clone()),
            &["notifications/payment_accepted"],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-a")), call_message())
            .await;
        assert_eq!(
            fx.harness.delivered(),
            1,
            "the paid request must be delivered even though the acceptance publish failed"
        );
        // The publish was attempted (ordered log) but never recorded as sent.
        assert!(fx
            .log
            .items()
            .contains(&"publish:notifications/payment_accepted".to_string()));
        assert_eq!(fx.sent.methods(), vec!["notifications/payment_required"]);

        // Post-invoice, verified: the entry is kept, so a redelivery does not re-charge.
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-a")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fx.harness.delivered(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_duplicates_charge_once_and_forward_once() {
        // Four genuinely concurrent dispatches per event id, iterated, on a multi-thread
        // runtime; the settle delay forces a suspension point inside the lifecycle. The
        // iteration count matters: a split-critical-section defect is a race, and a
        // single two-way attempt can be serialized by scheduler load and miss it.
        const ROUNDS: usize = 25;
        const RACERS: usize = 4;

        let log = EventLog::default();
        let fx = fixture(
            TestProcessor::new("pmi-a", log.clone())
                .with_verify(VerifyPlan::SettleAfter(Duration::from_millis(10))),
        );
        let fx = Arc::new(fx);

        for round in 0..ROUNDS {
            let event_id = format!("evt-c-{round}");
            let racers: Vec<_> = (0..RACERS)
                .map(|_| {
                    let fx = Arc::clone(&fx);
                    let event_id = event_id.clone();
                    tokio::spawn(async move {
                        fx.harness
                            .dispatch(Arc::new(test_ctx(&event_id)), call_message())
                            .await;
                    })
                })
                .collect();
            for racer in racers {
                racer.await.unwrap();
            }
            assert_eq!(
                fx.create_calls.load(Ordering::SeqCst),
                round + 1,
                "exactly one charge per event id, every round"
            );
        }

        // NOTE: no route-survival assertion here on purpose. The duplicates' chain runs
        // do not reach the terminal, so the seam pops the delivered request's route;
        // that is expected, and the snapshot fallback is what covers the response.
        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            ROUNDS,
            "one charge per id"
        );
        assert_eq!(
            fx.log.count_of("publish:notifications/payment_required"),
            ROUNDS,
            "one payment_required per id"
        );
        assert_eq!(fx.harness.delivered(), ROUNDS, "one forward per id");
    }

    #[tokio::test]
    async fn an_unpriced_request_is_forwarded_untouched() {
        let log = EventLog::default();
        let fx = fixture(TestProcessor::new("pmi-a", log.clone()));
        let message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("free-1"),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({ "name": "free-tool" })),
        });
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-free")), message)
            .await;
        assert_eq!(fx.harness.delivered(), 1);
        assert_eq!(fx.sent.items().len(), 0);
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_non_transparent_session_forwards_a_priced_request() {
        let log = EventLog::default();
        let fx = fixture(TestProcessor::new("pmi-a", log.clone()));
        let mut ctx = test_ctx("evt-gated");
        ctx.payment_interaction = Some(PaymentInteractionMode::ExplicitGating);
        fx.harness.dispatch(Arc::new(ctx), call_message()).await;
        assert_eq!(fx.harness.delivered(), 1);
        assert_eq!(fx.sent.items().len(), 0);
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_absent_mode_falls_through_to_the_priced_check() {
        // `None` must NOT forward early: it reaches the priced check and gates.
        // Reachable only from a hand-built context; the seam always resolves a mode.
        let log = EventLog::default();
        let fx = fixture(TestProcessor::new("pmi-a", log.clone()));
        let mut ctx = test_ctx("evt-none");
        ctx.payment_interaction = None;
        fx.harness.dispatch(Arc::new(ctx), call_message()).await;
        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            1,
            "the request was gated"
        );
        assert_eq!(fx.harness.delivered(), 1, "and forwarded after payment");
    }

    #[tokio::test]
    async fn a_notification_is_forwarded_even_when_its_method_is_priced() {
        let log = EventLog::default();
        let sent = Sent::default();
        let processor = TestProcessor::new("pmi-a", log.clone());
        let create_calls = Arc::clone(&processor.create_calls);
        // Price the notification's own method, so only the request gate can explain the
        // pass-through.
        let options = ServerPaymentsOptions::new(
            vec![Arc::new(processor)],
            vec![PricedCapability {
                method: "notifications/progress".to_string(),
                name: None,
                amount: 1,
                max_amount: None,
                currency_unit: "sats".to_string(),
                description: None,
            }],
        );
        let middleware = create_server_payments_middleware(ServerPaymentsMiddlewareParams::new(
            options,
            test_sender(log.clone(), sent.clone(), &[]),
        ));
        let harness = Harness::new(middleware, log);
        harness
            .dispatch(
                Arc::new(test_ctx("evt-notif")),
                JsonRpcMessage::Notification(JsonRpcNotification {
                    jsonrpc: "2.0".to_string(),
                    method: "notifications/progress".to_string(),
                    params: None,
                }),
            )
            .await;
        assert_eq!(harness.delivered(), 1);
        assert_eq!(sent.items().len(), 0);
        assert_eq!(create_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_entry_does_not_dedup_a_retry() {
        // The entry's expiry rides the pending TTL (10 s), NOT the polling bound the
        // processor's 1 s invoice TTL produces; the two clocks are deliberately set
        // apart so an entry stamped from the wrong one is caught on both sides.
        let log = EventLog::default();
        let fx = fixture_with(
            |o| o.with_payment_ttl(Duration::from_secs(10)),
            TestProcessor::new("pmi-a", log.clone()).with_ttl(1),
            &[],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-x")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 1);

        // Inside the pending TTL (but past the 1 s polling bound): still deduped.
        tokio::time::sleep(Duration::from_secs(5)).await;
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-x")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 1);

        // Past the pending TTL: the entry is expired and must NOT dedup the retry.
        tokio::time::sleep(Duration::from_secs(6)).await;
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-x")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_entry_beyond_the_purge_window_does_not_dedup() {
        // The lookup-time expiry check is the correctness gate; the purge is only
        // hygiene. In a small cache the purge removes an expired entry before the
        // lookup ever sees it, so this fixture stages the expired entry OUTSIDE the
        // purge's 25-deep scan from the least-recently-used end: only the lookup check
        // can now stop it from deduping a legitimate retry.
        fn entry(expires_at: Instant) -> PendingEntry {
            PendingEntry {
                expires_at,
                in_flight: async { Arc::new(Ok(())) }.boxed().shared(),
            }
        }

        let log = EventLog::default();
        let sent = Sent::default();
        let processor = TestProcessor::new("pmi-a", log.clone());
        let create_calls = Arc::clone(&processor.create_calls);
        let options = ServerPaymentsOptions::new(vec![Arc::new(processor)], vec![priced_call(21)]);
        let concrete = Arc::new(ServerPaymentsMiddleware {
            processors_by_pmi: Arc::new(build_processors_by_pmi(&options.processors)),
            sender: test_sender(log.clone(), sent, &[]),
            pending: tokio::sync::Mutex::new(LruCache::new(NonZeroUsize::new(40).unwrap())),
            options,
        });
        {
            let now = Instant::now();
            let mut cache = concrete.pending.lock().await;
            for i in 0..26 {
                cache.put(format!("live-{i}"), entry(now + Duration::from_secs(60)));
            }
            // Inserted last: most-recently-used, beyond the 25-entry LRU-end scan.
            cache.put(
                "expired-target".to_string(),
                entry(now - Duration::from_secs(1)),
            );
        }
        let harness = Harness::new(
            Arc::clone(&concrete) as Arc<dyn InboundMiddleware>,
            log.clone(),
        );

        harness
            .dispatch(Arc::new(test_ctx("expired-target")), call_message())
            .await;
        assert_eq!(
            create_calls.load(Ordering::SeqCst),
            1,
            "an expired entry must not dedup the retry, purge window or not"
        );
        assert_eq!(harness.delivered(), 1, "the retry runs the full lifecycle");
    }

    #[tokio::test(start_paused = true)]
    async fn the_purge_scans_the_least_recently_used_end() {
        fn completed_entry(expires_at: Instant) -> PendingEntry {
            PendingEntry {
                expires_at,
                in_flight: async { Arc::new(Ok(())) }.boxed().shared(),
            }
        }

        let mut cache: LruCache<String, PendingEntry> =
            LruCache::new(NonZeroUsize::new(40).unwrap());
        let now = Instant::now();
        // One expired entry inserted FIRST (so it sits at the LRU end)...
        cache.put(
            "old".to_string(),
            completed_entry(now - Duration::from_secs(1)),
        );
        // ...behind 30 live newer ones.
        for i in 0..30 {
            cache.put(
                format!("live-{i}"),
                completed_entry(now + Duration::from_secs(60)),
            );
        }

        purge_expired_pending(&mut cache, now);

        // A scan of the 25 most-recently-used entries can never see "old"; only the
        // LRU-end scan purges it.
        assert!(
            cache.peek("old").is_none(),
            "the expired LRU entry is purged"
        );
        assert_eq!(cache.len(), 30, "every live entry survives");
    }

    #[tokio::test(start_paused = true)]
    async fn the_verify_deadline_comes_from_the_payment_ttl_when_it_is_shorter() {
        // Invoice TTL 600 s vs pending TTL 200 ms: the verify must be abandoned at
        // 200 ms. Asserts WHEN the per-verify cancellation fired (observed by the
        // processor's background poller), not merely that a timeout happened.
        let log = EventLog::default();
        let processor = TestProcessor::new("pmi-a", log.clone())
            .with_ttl(600)
            .with_verify(VerifyPlan::Hang);
        let observed = Arc::clone(&processor.cancel_observed_after);
        let fx = fixture_with(
            |o| o.with_payment_ttl(Duration::from_millis(200)),
            processor,
            &[],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-dl1")), call_message())
            .await;
        tokio::time::sleep(Duration::from_millis(1)).await; // let the poller record
        assert_eq!(
            observed
                .lock()
                .unwrap()
                .expect("the cancellation must fire"),
            Duration::from_millis(200),
            "the deadline is the shorter pending TTL"
        );
        assert_eq!(fx.harness.delivered(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_verify_deadline_comes_from_the_payment_request_ttl_when_it_is_shorter() {
        // Invoice TTL 1 s vs pending TTL 300 s: the verify must be abandoned at 1 s.
        // Together with the sibling above this pins the deadline from both sides, and
        // pins that the timeout derives from the processor's ttl rather than always
        // using the default.
        let log = EventLog::default();
        let processor = TestProcessor::new("pmi-a", log.clone())
            .with_ttl(1)
            .with_verify(VerifyPlan::Hang);
        let observed = Arc::clone(&processor.cancel_observed_after);
        let fx = fixture_with(
            |o| o.with_payment_ttl(Duration::from_secs(300)),
            processor,
            &[],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-dl2")), call_message())
            .await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(
            observed
                .lock()
                .unwrap()
                .expect("the cancellation must fire"),
            Duration::from_secs(1),
            "the deadline is the invoice's own shorter TTL"
        );
        assert_eq!(fx.harness.delivered(), 0);
    }

    #[tokio::test]
    async fn an_empty_processor_list_drops_without_emitting() {
        let log = EventLog::default();
        let sent = Sent::default();
        let options = ServerPaymentsOptions::new(Vec::new(), vec![priced_call(1)]);
        let middleware = create_server_payments_middleware(ServerPaymentsMiddlewareParams::new(
            options,
            test_sender(log.clone(), sent.clone(), &[]),
        ));
        let harness = Harness::new(middleware, log);
        harness
            .dispatch(Arc::new(test_ctx("evt-none-proc")), call_message())
            .await;
        assert_eq!(harness.delivered(), 0, "a misconfigured gate fails closed");
        assert_eq!(sent.items().len(), 0, "and emits nothing");
    }

    #[tokio::test]
    async fn a_paid_request_redelivered_is_not_charged_again() {
        let log = EventLog::default();
        let fx = fixture(TestProcessor::new("pmi-a", log.clone()));
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-paid")), call_message())
            .await;
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-paid")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 1, "one charge");
        assert_eq!(fx.log.count_of("publish:notifications/payment_required"), 1);
        assert_eq!(fx.log.count_of("publish:notifications/payment_accepted"), 1);
        assert_eq!(fx.harness.delivered(), 1, "one delivery");
    }

    #[tokio::test]
    async fn a_waived_request_redelivered_is_not_forwarded_twice() {
        let log = EventLog::default();
        let fx = fixture_with(
            |o| {
                o.with_resolve_price(Arc::new(StaticResolver(ResolvePriceResult::Waive {
                    meta: None,
                })))
            },
            TestProcessor::new("pmi-a", log.clone()),
            &[],
        );
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-w2")), call_message())
            .await;
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-w2")), call_message())
            .await;
        assert_eq!(
            fx.harness.delivered(),
            1,
            "the waiver forwards exactly once"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_error_cleanup_does_not_pop_a_live_retrys_entry() {
        // Timeline (virtual): A inserts at t=0 (TTL 50 ms) and fails slowly at t=100;
        // a legitimate retry B inserts a LIVE entry at t=60; A's cleanup at t=100 must
        // pop only its OWN entry, so B's stays and dedups C at t=105.
        let log = EventLog::default();
        let fx = Arc::new(fixture_with(
            |o| o.with_payment_ttl(Duration::from_millis(50)),
            TestProcessor::new("pmi-a", log.clone()).with_create_plan(vec![
                CreatePlan::ErrAfter(Duration::from_millis(100)),
                CreatePlan::OkAfter(Duration::from_millis(100)),
            ]),
            &[],
        ));

        let a = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move {
                fx.harness
                    .dispatch(Arc::new(test_ctx("evt-aba")), call_message())
                    .await;
            })
        };
        tokio::time::sleep(Duration::from_millis(60)).await;
        let b = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move {
                fx.harness
                    .dispatch(Arc::new(test_ctx("evt-aba")), call_message())
                    .await;
            })
        };
        tokio::time::sleep(Duration::from_millis(45)).await; // t = 105
        let c = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move {
                fx.harness
                    .dispatch(Arc::new(test_ctx("evt-aba")), call_message())
                    .await;
            })
        };
        a.await.unwrap();
        b.await.unwrap();
        c.await.unwrap();

        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            2,
            "A and B each ran; C joined B's live entry instead of starting a third"
        );
        assert_eq!(fx.harness.delivered(), 1, "B's lifecycle forwarded once");
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_pressure_never_evicts_a_live_pending_entry() {
        let log = EventLog::default();
        let fx = Arc::new(fixture_with(
            |o| o.with_max_pending_payments(2),
            TestProcessor::new("pmi-a", log.clone())
                .with_verify(VerifyPlan::SettleAfter(Duration::from_millis(100))),
            &[],
        ));

        let a = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move {
                fx.harness
                    .dispatch(Arc::new(test_ctx("evt-cap-a")), call_message())
                    .await;
            })
        };
        let b = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move {
                fx.harness
                    .dispatch(Arc::new(test_ctx("evt-cap-b")), call_message())
                    .await;
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Cache full, both entries live: the third id is refused, not started, and no
        // live entry is evicted for it.
        let fx2 = Arc::clone(&fx);
        fx2.harness
            .dispatch(Arc::new(test_ctx("evt-cap-c")), call_message())
            .await;

        a.await.unwrap();
        b.await.unwrap();

        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 2, "no third charge");
        assert_eq!(
            fx.harness.delivered(),
            2,
            "the two live payments both deliver"
        );

        // Both surviving entries still dedup their ids.
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-cap-a")), call_message())
            .await;
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-cap-b")), call_message())
            .await;
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_cache_evicts_the_expired_entry_not_the_live_lru_one() {
        fn entry(expires_at: Instant) -> PendingEntry {
            PendingEntry {
                expires_at,
                in_flight: async { Arc::new(Ok(())) }.boxed().shared(),
            }
        }

        // Build the middleware directly so the cache can be staged: 30 slots, all live
        // except ONE expired entry positioned among the 5 most-recently-used, which the
        // 25-deep LRU-end purge can never reach.
        let log = EventLog::default();
        let sent = Sent::default();
        let processor = TestProcessor::new("pmi-a", log.clone());
        let create_calls = Arc::clone(&processor.create_calls);
        let options = ServerPaymentsOptions::new(vec![Arc::new(processor)], vec![priced_call(21)])
            .with_max_pending_payments(30);
        let concrete = Arc::new(ServerPaymentsMiddleware {
            processors_by_pmi: Arc::new(build_processors_by_pmi(&options.processors)),
            sender: test_sender(log.clone(), sent.clone(), &[]),
            pending: tokio::sync::Mutex::new(LruCache::new(NonZeroUsize::new(30).unwrap())),
            options,
        });
        {
            let now = Instant::now();
            let mut cache = concrete.pending.lock().await;
            cache.put("lru-live".to_string(), entry(now + Duration::from_secs(60)));
            for i in 0..24 {
                cache.put(format!("live-{i}"), entry(now + Duration::from_secs(60)));
            }
            cache.put(
                "expired-mru".to_string(),
                entry(now - Duration::from_secs(1)),
            );
            for i in 24..28 {
                cache.put(format!("live-{i}"), entry(now + Duration::from_secs(60)));
            }
            assert_eq!(cache.len(), 30);
        }

        let harness = Harness::new(
            Arc::clone(&concrete) as Arc<dyn InboundMiddleware>,
            log.clone(),
        );
        harness
            .dispatch(Arc::new(test_ctx("fresh")), call_message())
            .await;

        {
            let cache = concrete.pending.lock().await;
            assert!(
                cache.peek("lru-live").is_some(),
                "the live LRU-end entry must survive the capacity insert"
            );
            assert!(
                cache.peek("expired-mru").is_none(),
                "the expired entry is the one evicted"
            );
            assert!(cache.peek("fresh").is_some(), "the new payment was tracked");
        }
        assert_eq!(create_calls.load(Ordering::SeqCst), 1);

        // The surviving LRU entry still dedups: its id is never charged.
        harness
            .dispatch(Arc::new(test_ctx("lru-live")), call_message())
            .await;
        assert_eq!(
            create_calls.load(Ordering::SeqCst),
            1,
            "the live LRU entry's dedup stayed armed"
        );
    }

    #[test]
    fn purge_scan_limit_matches_ts_sdk() {
        // The purge inspects at most 25 entries per lookup, the reference
        // implementation's cap. The limit is hygiene, not correctness (the lookup-time
        // expiry check carries that), so it is pinned by value rather than by behavior.
        assert_eq!(PENDING_PURGE_SCAN_LIMIT, 25);
    }

    #[tokio::test(start_paused = true)]
    async fn the_lookup_purge_drops_expired_entries() {
        // Distinct one-shot ids leave dead entries behind; the purge on the next
        // lookup is what keeps them from accumulating until capacity pressure. Built
        // concretely so the cache length is observable.
        let log = EventLog::default();
        let sent = Sent::default();
        let processor = TestProcessor::new("pmi-a", log.clone());
        let options = ServerPaymentsOptions::new(vec![Arc::new(processor)], vec![priced_call(21)])
            .with_payment_ttl(Duration::from_millis(50));
        let concrete = Arc::new(ServerPaymentsMiddleware {
            processors_by_pmi: Arc::new(build_processors_by_pmi(&options.processors)),
            sender: test_sender(log.clone(), sent, &[]),
            pending: tokio::sync::Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
            options,
        });
        let harness = Harness::new(
            Arc::clone(&concrete) as Arc<dyn InboundMiddleware>,
            log.clone(),
        );

        for i in 0..3 {
            harness
                .dispatch(Arc::new(test_ctx(&format!("evt-spam-{i}"))), call_message())
                .await;
        }
        assert_eq!(concrete.pending.lock().await.len(), 3);
        tokio::time::sleep(Duration::from_millis(100)).await;

        harness
            .dispatch(Arc::new(test_ctx("evt-after")), call_message())
            .await;
        assert_eq!(
            concrete.pending.lock().await.len(),
            1,
            "the lookup purge must drop the expired one-shot entries, not leave them \
             for capacity pressure"
        );
    }

    #[tokio::test]
    async fn a_panicking_processor_does_not_poison_the_entry_for_the_ttl() {
        let log = EventLog::default();
        let fx = fixture(
            TestProcessor::new("pmi-a", log.clone())
                .with_create_plan(vec![CreatePlan::Panic, CreatePlan::Ok]),
        );
        // The panic happens before any invoice, so the entry is deleted and the retry
        // re-runs; neither dispatch propagates a panic into this test.
        fx.harness
            .dispatch(Arc::new(test_ctx("evt-panic")), call_message())
            .await;
        assert_eq!(fx.harness.delivered(), 0);

        fx.harness
            .dispatch(Arc::new(test_ctx("evt-panic")), call_message())
            .await;
        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            2,
            "the retry re-ran"
        );
        assert_eq!(fx.harness.delivered(), 1, "and completed normally");
    }
}
