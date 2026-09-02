//! CEP-8 explicit-gating server payment middleware.
//!
//! A priced invocation in an `explicit_gating` session is answered immediately with a
//! JSON-RPC error and dropped: `-32042 Payment Required` carrying one payment option on
//! the first sighting, `-32043 Payment Pending` on a duplicate while the payment is in
//! flight. A detached background task verifies the payment and records a single-use
//! grant in the [`AuthorizationStore`], keyed by canonical invocation identity; a later
//! invocation with the same identity claims the grant atomically and is forwarded to
//! the MCP handler. The gated request itself is never forwarded (CEP-8's non-forwarding
//! MUST); the result rides the claiming invocation.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::FutureExt; // .catch_unwind()

use crate::core::types::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, PaymentInteractionMode,
};
use crate::payments::authorization_store::{AuthorizationStore, ClaimOrPending};
use crate::payments::canonical::{
    compute_canonical_invocation_identity, CanonicalInvocationIdentity,
};
use crate::payments::constants::{PAYMENT_PENDING_ERROR_CODE, PAYMENT_REQUIRED_ERROR_CODE};
use crate::payments::server_payments::ServerPaymentsOptions;
use crate::payments::server_payments_utils::{
    build_processors_by_pmi, match_priced_capability, resolve_and_initiate_payment,
    InitiationOutcome,
};
use crate::payments::traits::PaymentProcessor;
use crate::payments::types::{
    PaymentOption, PaymentPendingErrorData, PaymentProcessorVerifyParams, PaymentRequiredErrorData,
};
use crate::transport::server::middleware::{InboundContext, InboundMiddleware, Next};
use crate::transport::server::TargetedResponseSender;

const LOG_TARGET: &str = "contextvm_sdk::payments::server_explicit_gating";

/// The `-32042` instructions string, byte-identical to the reference implementation.
const PAYMENT_REQUIRED_INSTRUCTIONS: &str = "Payment is required to process this request. Pay one \
     of the offered options, then retry the same request with exactly the same method and params.";

/// The `-32043` instructions string, byte-identical to the reference implementation.
const PAYMENT_PENDING_INSTRUCTIONS: &str = "A payment is already pending for this invocation. \
     Retry the same request later with exactly the same method and params.";

/// The rejection fallback message when the pricing callback gives no reason.
const PAYMENT_REJECTED_FALLBACK_MESSAGE: &str = "Payment rejected by policy";

/// Inputs to [`create_explicit_gating_middleware`].
///
/// A struct rather than a positional list so later payment work can add fields without a
/// breaking signature change (the same shape as
/// [`ServerPaymentsMiddlewareParams`](crate::payments::ServerPaymentsMiddlewareParams)).
#[non_exhaustive]
pub struct ExplicitGatingMiddlewareParams {
    /// The payment configuration (processors, priced capabilities, TTLs). Shared with the
    /// transparent middleware: both lifecycles read the same options object.
    pub options: ServerPaymentsOptions,
    /// The transport's injected route-bypassing targeted-response publish
    /// ([`NostrServerTransport::targeted_response_sender`](crate::transport::server::NostrServerTransport::targeted_response_sender)).
    pub sender: TargetedResponseSender,
    /// The store of pending and granted payment authorizations. Injected, not constructed
    /// here, so the configuration entry point can share one store across registrations and
    /// tests can observe lifecycle state.
    pub authorization_store: AuthorizationStore,
    /// Pre-built PMI-to-processor map, shared across middlewares. Built locally when `None`.
    pub processors_by_pmi: Option<Arc<HashMap<String, Arc<dyn PaymentProcessor>>>>,
}

impl ExplicitGatingMiddlewareParams {
    /// Build params from the three required inputs, with no shared processor map.
    pub fn new(
        options: ServerPaymentsOptions,
        sender: TargetedResponseSender,
        authorization_store: AuthorizationStore,
    ) -> Self {
        Self {
            options,
            sender,
            authorization_store,
            processors_by_pmi: None,
        }
    }
}

/// Create the CEP-8 explicit-gating payment middleware.
///
/// Three self-gates run before any payment work, each forwarding the message untouched:
/// a non-request is never gated; a session whose negotiated payment-interaction mode is
/// anything other than `explicit_gating` (including an unresolved mode) is another
/// lifecycle's business; and an unpriced request is free. Everything else runs the
/// gating lifecycle: consume a live grant and forward, park a duplicate behind a live
/// pending with `-32043`, or answer `-32042` with exactly one payment option, drop the
/// request, and verify the payment on a detached task that grants on success.
///
/// ## Shutdown
///
/// The detached verification runs under a child of the per-event cancellation token, so
/// closing the transport cancels an in-flight verification: a conforming processor
/// returns an error, the task clears the pending state and exits. `close()` does not
/// wait for that exit; the store is process-local, so the only effect of the unawaited
/// tail is an in-flight instruction window, never an external one.
///
/// ## Bounds and loss modes
///
/// One detached verification task is spawned per successful offer; the per-identity
/// pending state dedups repeats, but distinct identities fan out without any further
/// bound (rate limiting is deliberately out of scope here). The store is in-memory and
/// single-process: a restart forgets every pending and granted authorization, and an
/// LRU-evicted entry (5000 per map) silently re-invoices its payer on retry. A `-32043`
/// aimed at an evicted session is a logged no-op (the sender's contract), and the client
/// polls into silence until the pending expires and a fresh offer is minted. A failed
/// first-offer publish also costs the session its one-shot discovery-replay and
/// disclosure latches (the sender consumes them before publishing and does not restore
/// them on failure).
pub fn create_explicit_gating_middleware(
    params: ExplicitGatingMiddlewareParams,
) -> Arc<dyn InboundMiddleware> {
    let processors_by_pmi = params
        .processors_by_pmi
        .unwrap_or_else(|| Arc::new(build_processors_by_pmi(&params.options.processors)));
    Arc::new(ExplicitGatingMiddleware {
        options: params.options,
        sender: params.sender,
        store: params.authorization_store,
        processors_by_pmi,
    })
}

/// Build the full `-32042 Payment Required` error response.
///
/// This is the middleware's single composition site for the offer payload, and it takes
/// exactly one [`PaymentOption`] by value: the CEP-8 requirement that `payment_options`
/// carry at least one entry holds by construction, because no caller can hand it an
/// empty list.
pub fn build_payment_required_error(
    id: serde_json::Value,
    option: PaymentOption,
) -> JsonRpcErrorResponse {
    let data = PaymentRequiredErrorData {
        instructions: Some(PAYMENT_REQUIRED_INSTRUCTIONS.to_string()),
        payment_options: vec![option],
    };
    JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        id,
        error: JsonRpcError {
            code: PAYMENT_REQUIRED_ERROR_CODE,
            message: "Payment Required".to_string(),
            data: Some(
                serde_json::to_value(&data)
                    .expect("strings, integers and Value maps serialize infallibly"),
            ),
        },
    }
}

/// Build the full `-32043 Payment Pending` error response for a payment with
/// `remaining_ms` left on its pending window.
///
/// `retry_after` is a poll-interval suggestion clamped to the band `[1, 2]` seconds,
/// not the remaining TTL, matching the reference implementation's formula.
pub fn build_payment_pending_error(
    id: serde_json::Value,
    remaining_ms: u64,
) -> JsonRpcErrorResponse {
    let data = PaymentPendingErrorData {
        instructions: Some(PAYMENT_PENDING_INSTRUCTIONS.to_string()),
        retry_after: Some(retry_after_secs(remaining_ms)),
    };
    JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        id,
        error: JsonRpcError {
            code: PAYMENT_PENDING_ERROR_CODE,
            message: "Payment Pending".to_string(),
            data: Some(
                serde_json::to_value(&data).expect("strings and integers serialize infallibly"),
            ),
        },
    }
}

/// Build the rejection error response for a priced request the pricing callback refused.
///
/// CEP-8 defines no rejection code for the gating lifecycle, so this reuses the generic
/// server-error `-32000` with the policy message and **no** `data` key, exactly as the
/// reference implementation does (the literal code is deliberate; there is no named
/// constant to consume).
pub fn build_payment_rejected_error(
    id: serde_json::Value,
    message: Option<String>,
) -> JsonRpcErrorResponse {
    JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        id,
        error: JsonRpcError {
            code: -32000,
            message: message.unwrap_or_else(|| PAYMENT_REJECTED_FALLBACK_MESSAGE.to_string()),
            data: None,
        },
    }
}

/// `retry_after` for a pending payment: `ceil(remaining_ms / 1000)` clamped to `[1, 2]`.
fn retry_after_secs(remaining_ms: u64) -> u64 {
    remaining_ms.div_ceil(1000).clamp(1, 2)
}

// Payload-logging rule, load-bearing for money privacy: `pay_req` (an invoice / payment
// request string) must never reach a log line. The `-32042` payload embeds `pay_req`
// inside `payment_options`, and `PaymentRequiredParams`, `PaymentOption`,
// `PaymentRequiredErrorData`, `InitiationOutcome` and the error responses all derive or
// contain `Debug`-formattable carriers of it, so no log statement may serialize or
// Debug-format any of them. Loggable fields are the allowlist: pmi, amount, ttl,
// request event id, method, and a failure class. A caught panic payload is logged as a
// class only, never formatted, since it can carry processor state.
struct ExplicitGatingMiddleware {
    options: ServerPaymentsOptions,
    sender: TargetedResponseSender,
    store: AuthorizationStore,
    processors_by_pmi: Arc<HashMap<String, Arc<dyn PaymentProcessor>>>,
}

#[async_trait]
impl InboundMiddleware for ExplicitGatingMiddleware {
    async fn handle(&self, message: JsonRpcMessage, ctx: &InboundContext, next: Next) -> bool {
        // Self-gate 1: only requests are gated; notifications and responses pass.
        let request = match &message {
            JsonRpcMessage::Request(request) => request.clone(),
            _ => return next.run(message).await,
        };

        // Self-gate 2: only a session negotiated onto explicit gating engages here.
        // Deliberately NOT the mirror image of the transparent gate: there an absent
        // mode falls through to the priced check, while here it forwards, both matching
        // the reference implementation's comparisons. In production the seam always
        // resolves the mode, so the `None` arm is reachable only from a hand-built
        // context; it still matters, because flipping it would gate every un-negotiated
        // session.
        if ctx.payment_interaction != Some(PaymentInteractionMode::ExplicitGating) {
            return next.run(message).await;
        }

        // Self-gate 3: an unpriced request is free.
        let Some(priced) =
            match_priced_capability(&request, &self.options.priced_capabilities).cloned()
        else {
            return next.run(message).await;
        };

        // Step 0: canonical invocation identity. On failure, drop and fail closed: a
        // request whose identity cannot be computed cannot be gated, and forwarding
        // would execute a priced capability for free. No state exists yet, so there is
        // nothing to clean.
        let identity = match compute_canonical_invocation_identity(
            &ctx.client_pubkey,
            &request.method,
            request.params.as_ref(),
        ) {
            Ok(identity) => identity,
            Err(error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    method = %request.method,
                    request_event_id = %ctx.request_event_id,
                    error = %error,
                    "cannot canonicalize a priced invocation; dropping it (fail closed)"
                );
                return false;
            }
        };

        let payment_ttl_ms =
            u64::try_from(self.options.payment_ttl.as_millis()).unwrap_or(u64::MAX);

        // Step 1: one composed store call. A grant landing between a separate claim and
        // set-pending would mint a spurious invoice against an unclaimed paid grant, so
        // the two checks share one critical section.
        match self.store.claim_or_set_pending(&identity, payment_ttl_ms) {
            ClaimOrPending::Claimed => {
                tracing::info!(
                    target: LOG_TARGET,
                    method = %request.method,
                    request_event_id = %ctx.request_event_id,
                    "claimed a paid authorization; forwarding the invocation"
                );
                // The ORIGINAL message, untouched: execution must see the full params
                // including `_meta` (the identity strip happens on a copy inside the
                // hash function, never on the forwarded message).
                return next.run(message).await;
            }
            ClaimOrPending::AlreadyPending { remaining_ms } => {
                let response = build_payment_pending_error(request.id.clone(), remaining_ms);
                if let Err(error) = (self.sender)(
                    ctx.client_pubkey.clone(),
                    ctx.request_event_id.clone(),
                    JsonRpcMessage::ErrorResponse(response),
                )
                .await
                {
                    // The pending entry belongs to the live original offer cycle;
                    // clearing it here would let a duplicate mint a competing invoice
                    // while that cycle's verification is in flight. Leave it untouched.
                    tracing::warn!(
                        target: LOG_TARGET,
                        request_event_id = %ctx.request_event_id,
                        error = %error,
                        "failed to publish the payment-pending error"
                    );
                }
                return false;
            }
            ClaimOrPending::PendingSet => {
                // This task now owns the offer cycle; continue.
            }
        }

        // Step 2: select a processor and resolve the price.
        let outcome = resolve_and_initiate_payment(
            &request,
            &priced,
            &ctx.request_event_id,
            &ctx.client_pubkey,
            ctx.client_pmis.as_deref(),
            &self.options,
            &self.processors_by_pmi,
        )
        .await;

        let (processor, payment_required, merged_meta, verify_timeout) = match outcome {
            Err(error) => {
                self.store.clear_pending(&identity);
                tracing::warn!(
                    target: LOG_TARGET,
                    method = %request.method,
                    request_event_id = %ctx.request_event_id,
                    error = %error,
                    "payment initiation failed; dropping the gated request"
                );
                return false;
            }
            Ok(InitiationOutcome::Rejected {
                pmi,
                amount,
                message: reason,
            }) => {
                self.store.clear_pending(&identity);
                tracing::info!(
                    target: LOG_TARGET,
                    request_event_id = %ctx.request_event_id,
                    pmi = %pmi,
                    amount,
                    "payment rejected by policy"
                );
                let response = build_payment_rejected_error(request.id.clone(), reason);
                if let Err(error) = (self.sender)(
                    ctx.client_pubkey.clone(),
                    ctx.request_event_id.clone(),
                    JsonRpcMessage::ErrorResponse(response),
                )
                .await
                {
                    // Pending was already cleared before the publish; nothing else to
                    // unwind.
                    tracing::warn!(
                        target: LOG_TARGET,
                        request_event_id = %ctx.request_event_id,
                        error = %error,
                        "failed to publish the payment-rejected error"
                    );
                }
                return false;
            }
            Ok(InitiationOutcome::Waived) => {
                self.store.clear_pending(&identity);
                tracing::debug!(
                    target: LOG_TARGET,
                    request_event_id = %ctx.request_event_id,
                    "payment waived, forwarding priced request"
                );
                return next.run(message).await;
            }
            Ok(InitiationOutcome::PaymentRequired {
                processor,
                payment_required,
                merged_meta,
                verify_timeout,
            }) => (processor, payment_required, merged_meta, verify_timeout),
        };

        // Step 3: TTL bookkeeping. A `ttl: 0` option is treated as absent (a declared
        // divergence from the reference implementation, which mints a grant that
        // expires at birth while the invoice stays payable); the sibling
        // verification-timeout rule already treats `ttl <= 0` as absent, so this
        // applies the same reading consistently. `saturating_mul` keeps a hostile ttl
        // from overflowing.
        let grant_ttl_ms = payment_required
            .ttl
            .filter(|s| *s > 0)
            .map(|s| s.saturating_mul(1000))
            .unwrap_or(payment_ttl_ms);
        let polling_timeout = verify_timeout.min(self.options.payment_ttl);
        // Tighten (or widen) the pending window from the safe default to the offer's
        // own horizon, before the offer publishes. The no-op-on-expired guard does not
        // protect a NEW cycle's pending from a stale cross-cycle retime: if this
        // cycle's pending expired during a slow initiate and a concurrent retry
        // already re-pended, this call retimes the new cycle's entry. Bounded
        // consequence (skewed retry hints, an overlapping double offer) and identical
        // mechanics in the reference implementation; declared, not fixed.
        self.store.update_pending_ttl(&identity, grant_ttl_ms);

        // Step 4: the offer, built at the single composition site and published through
        // the route-bypassing targeted sender with the ORIGINAL inner request id (the
        // targeted path does not rewrite ids; the spec's examples show the request's
        // own id).
        let response = build_payment_required_error(
            request.id.clone(),
            PaymentOption {
                amount: payment_required.amount,
                pmi: payment_required.pmi.clone(),
                pay_req: payment_required.pay_req.clone(),
                description: payment_required.description.clone(),
                ttl: payment_required.ttl,
                meta: merged_meta,
            },
        );
        tracing::info!(
            target: LOG_TARGET,
            request_event_id = %ctx.request_event_id,
            pmi = %payment_required.pmi,
            amount = payment_required.amount,
            ttl = payment_required.ttl,
            "answering a priced invocation with a payment-required error"
        );
        if let Err(error) = (self.sender)(
            ctx.client_pubkey.clone(),
            ctx.request_event_id.clone(),
            JsonRpcMessage::ErrorResponse(response),
        )
        .await
        {
            // No offer reached the client, so nothing may block its retry: clear the
            // pending so the next identical invocation starts a fresh offer cycle, and
            // spawn NO verification (there is no invoice to verify).
            self.store.clear_pending(&identity);
            tracing::warn!(
                target: LOG_TARGET,
                request_event_id = %ctx.request_event_id,
                pmi = %payment_required.pmi,
                error = %error,
                "failed to publish the payment-required error; cleared pending state"
            );
            return false;
        }

        // Step 5: detached verification, spawned only after the offer is on the wire.
        spawn_verification(
            processor,
            payment_required.pay_req.clone(),
            payment_required.pmi.clone(),
            payment_required.amount,
            identity,
            self.store.clone(),
            grant_ttl_ms,
            polling_timeout,
            ctx,
        );

        // The gated request is dropped: the offer answered it, and the result rides a
        // later claiming invocation.
        false
    }
}

/// Spawn the detached verification task for one published offer.
///
/// The task runs under a **child** of the per-event cancellation token, never the
/// per-event token itself: that token is shared by every middleware on the event and is
/// also the transport-shutdown cascade, so this task may cancel only its own child. The
/// child is cancelled on every exit path (the abort-on-settle contract for processors
/// that spawned their own pollers). Any processor `Ok` grants; an error, an elapsed
/// timeout, a caught panic, or a cancellation clears the pending state so a retry can
/// mint a fresh offer.
#[allow(clippy::too_many_arguments)]
fn spawn_verification(
    processor: Arc<dyn PaymentProcessor>,
    pay_req: String,
    pmi: String,
    amount: i64,
    identity: CanonicalInvocationIdentity,
    store: AuthorizationStore,
    grant_ttl_ms: u64,
    polling_timeout: std::time::Duration,
    ctx: &InboundContext,
) {
    let request_event_id = ctx.request_event_id.clone();
    let client_pubkey = ctx.client_pubkey.clone();
    let verify_cancel = ctx.cancel.child_token();

    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(tokio::time::timeout(
            polling_timeout,
            processor.verify_payment(PaymentProcessorVerifyParams {
                pay_req,
                request_event_id: request_event_id.clone(),
                client_pubkey,
                cancel: verify_cancel.clone(),
            }),
        ))
        .catch_unwind()
        .await;
        // The `finally` analogue: fired on every exit path, including settlement, so a
        // processor that spawned its own background poller observes the abort.
        verify_cancel.cancel();

        match outcome {
            // Verification success is `Ok` through all three layers; any processor
            // `Ok` grants (the outcome carries no discriminant).
            Ok(Ok(Ok(_verified))) => {
                store.grant(&identity, grant_ttl_ms);
                tracing::info!(
                    target: LOG_TARGET,
                    request_event_id = %request_event_id,
                    pmi = %pmi,
                    amount,
                    "payment verified; granted a single-use authorization"
                );
            }
            Ok(Ok(Err(error))) => {
                store.clear_pending(&identity);
                tracing::info!(
                    target: LOG_TARGET,
                    request_event_id = %request_event_id,
                    pmi = %pmi,
                    error = %error,
                    "payment verification failed; cleared pending state"
                );
            }
            Ok(Err(_elapsed)) => {
                store.clear_pending(&identity);
                tracing::info!(
                    target: LOG_TARGET,
                    request_event_id = %request_event_id,
                    pmi = %pmi,
                    "payment verification timed out; cleared pending state"
                );
            }
            Err(_panic) => {
                // The payload is logged as a failure class only: a panic payload can
                // carry processor state.
                store.clear_pending(&identity);
                tracing::error!(
                    target: LOG_TARGET,
                    request_event_id = %request_event_id,
                    pmi = %pmi,
                    "payment verification panicked; cleared pending state"
                );
            }
        }
    });
}

// Test clocks: the store expires on `std::time::Instant`, which tokio's paused clock
// does not advance, so every store-TTL assertion below runs in real time (tiny TTLs on
// expired-direction assertions, >= 100 ms on live reads). No test here uses
// `start_paused`. Every async test names its runtime flavor explicitly.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{JsonRpcNotification, JsonRpcRequest};
    use crate::payments::errors::PaymentError;
    use crate::payments::traits::ResolvePrice;
    use crate::payments::types::{
        PaymentProcessorCreateParams, PaymentRequiredParams, PricedCapability, ResolvePriceParams,
        ResolvePriceResult, VerifyOutcome,
    };
    use crate::transport::open_stream::OpenStreamConfig;
    use crate::transport::server::middleware::run_inbound_chain;
    use crate::transport::server::{IncomingRequest, ServerEventRouteStore, ServerOpenStreamState};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
    use tokio::sync::Semaphore;
    use tokio_util::sync::CancellationToken;

    // ── local doubles ───────────────────────────────────────────────

    /// Successfully published targeted responses: (client pubkey, event id, response).
    #[derive(Clone, Default)]
    struct Sent(Arc<StdMutex<Vec<(String, String, JsonRpcErrorResponse)>>>);

    impl Sent {
        fn responses(&self) -> Vec<JsonRpcErrorResponse> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(_, _, r)| r.clone())
                .collect()
        }
        fn codes(&self) -> Vec<i64> {
            self.responses().iter().map(|r| r.error.code).collect()
        }
    }

    /// A recording targeted-sender double; responses whose error code is in
    /// `fail_codes` return `Err` without being recorded as sent (the attempt counter
    /// still ticks).
    fn test_sender(
        sent: Sent,
        attempts: Arc<AtomicUsize>,
        fail_codes: &'static [i64],
    ) -> TargetedResponseSender {
        Arc::new(move |client_pubkey, event_id, message| {
            let sent = sent.clone();
            let attempts = Arc::clone(&attempts);
            Box::pin(async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                if let JsonRpcMessage::ErrorResponse(response) = &message {
                    if fail_codes.contains(&response.error.code) {
                        return Err(crate::Error::Other("publish failed".to_string()));
                    }
                    sent.0
                        .lock()
                        .unwrap()
                        .push((client_pubkey, event_id, response.clone()));
                }
                Ok(())
            })
        })
    }

    enum VerifyPlan {
        /// Settle immediately.
        InstantOk,
        /// Fail immediately.
        InstantErr,
        /// Panic immediately (a crashing processor).
        Panic,
        /// Park until the test releases a semaphore permit (then settle), or the
        /// per-verify cancellation fires (then fail), whichever happens first.
        Gated,
    }

    /// A controllable processor double. Every verify call stashes a clone of its
    /// cancellation token so tests can observe the completion-path cancel.
    struct GateProcessor {
        pmi: String,
        ttl: Option<u64>,
        plan: VerifyPlan,
        create_calls: Arc<AtomicUsize>,
        verify_calls: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
        stashed_cancels: Arc<StdMutex<Vec<CancellationToken>>>,
    }

    impl GateProcessor {
        fn new(pmi: &str) -> Self {
            Self {
                pmi: pmi.to_string(),
                ttl: None,
                plan: VerifyPlan::InstantOk,
                create_calls: Arc::new(AtomicUsize::new(0)),
                verify_calls: Arc::new(AtomicUsize::new(0)),
                release: Arc::new(Semaphore::new(0)),
                stashed_cancels: Arc::new(StdMutex::new(Vec::new())),
            }
        }
        fn with_ttl(mut self, ttl: u64) -> Self {
            self.ttl = Some(ttl);
            self
        }
        fn with_plan(mut self, plan: VerifyPlan) -> Self {
            self.plan = plan;
            self
        }
    }

    #[async_trait]
    impl PaymentProcessor for GateProcessor {
        fn pmi(&self) -> &str {
            &self.pmi
        }

        async fn create_payment_required(
            &self,
            params: PaymentProcessorCreateParams,
        ) -> Result<PaymentRequiredParams, PaymentError> {
            let n = self.create_calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(PaymentRequiredParams {
                amount: params.amount,
                pay_req: format!("invoice-{}-{n}", params.request_event_id),
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
            self.verify_calls.fetch_add(1, Ordering::SeqCst);
            self.stashed_cancels
                .lock()
                .unwrap()
                .push(params.cancel.clone());
            match self.plan {
                VerifyPlan::InstantOk => Ok(VerifyOutcome::default()),
                VerifyPlan::InstantErr => Err(PaymentError::Processor("verify failed".to_string())),
                VerifyPlan::Panic => panic!("processor crashed mid-verification"),
                VerifyPlan::Gated => tokio::select! {
                    _ = params.cancel.cancelled() => {
                        Err(PaymentError::Processor("verification cancelled".to_string()))
                    }
                    permit = self.release.acquire() => {
                        permit.expect("release semaphore is never closed").forget();
                        Ok(VerifyOutcome::default())
                    }
                },
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

    // ── harness ─────────────────────────────────────────────────────

    struct Fixture {
        middleware: Arc<dyn InboundMiddleware>,
        routes: ServerEventRouteStore,
        open_stream: ServerOpenStreamState,
        tx: UnboundedSender<IncomingRequest>,
        rx: StdMutex<UnboundedReceiver<IncomingRequest>>,
        sent: Sent,
        attempts: Arc<AtomicUsize>,
        store: AuthorizationStore,
        create_calls: Arc<AtomicUsize>,
        verify_calls: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
        stashed_cancels: Arc<StdMutex<Vec<CancellationToken>>>,
    }

    impl Fixture {
        /// Run the real chain to completion for one event (the production entry point;
        /// a middleware called as a bare function would skip the seam's drop-cleanup).
        async fn dispatch(&self, ctx: InboundContext, message: JsonRpcMessage) {
            let chain: Arc<[Arc<dyn InboundMiddleware>]> =
                Arc::from(vec![Arc::clone(&self.middleware)]);
            run_inbound_chain(
                chain,
                Arc::new(ctx),
                self.tx.clone(),
                self.routes.clone(),
                self.open_stream.clone(),
                Arc::new(tokio::sync::RwLock::new(HashMap::new())),
                message,
                None,
            )
            .await;
        }

        /// Drain and return everything that reached the worker channel so far.
        fn delivered(&self) -> Vec<IncomingRequest> {
            let mut rx = self.rx.lock().unwrap();
            let mut out = Vec::new();
            while let Ok(item) = rx.try_recv() {
                out.push(item);
            }
            out
        }
    }

    fn build_fixture(
        processor: GateProcessor,
        configure: impl FnOnce(ServerPaymentsOptions) -> ServerPaymentsOptions,
        fail_codes: &'static [i64],
    ) -> Fixture {
        let create_calls = Arc::clone(&processor.create_calls);
        let verify_calls = Arc::clone(&processor.verify_calls);
        let release = Arc::clone(&processor.release);
        let stashed_cancels = Arc::clone(&processor.stashed_cancels);
        let options = configure(ServerPaymentsOptions::new(
            vec![Arc::new(processor)],
            vec![priced("tools/call", Some("echo"), 21)],
        ));
        let sent = Sent::default();
        let attempts = Arc::new(AtomicUsize::new(0));
        let store = AuthorizationStore::new();
        let middleware = create_explicit_gating_middleware(ExplicitGatingMiddlewareParams::new(
            options,
            test_sender(sent.clone(), Arc::clone(&attempts), fail_codes),
            store.clone(),
        ));
        let (tx, rx) = mpsc::unbounded_channel();
        Fixture {
            middleware,
            routes: ServerEventRouteStore::new(),
            open_stream: ServerOpenStreamState::new(&OpenStreamConfig::default(), 10),
            tx,
            rx: StdMutex::new(rx),
            sent,
            attempts,
            store,
            create_calls,
            verify_calls,
            release,
            stashed_cancels,
        }
    }

    fn fixture(processor: GateProcessor) -> Fixture {
        build_fixture(processor, |o| o, &[])
    }

    fn priced(method: &str, name: Option<&str>, amount: i64) -> PricedCapability {
        PricedCapability {
            method: method.to_string(),
            name: name.map(str::to_string),
            amount,
            max_amount: None,
            currency_unit: "sats".to_string(),
            description: None,
        }
    }

    fn client_pk() -> String {
        "c".repeat(64)
    }

    fn test_ctx(event_id: &str) -> InboundContext {
        InboundContext {
            client_pubkey: client_pk(),
            request_event_id: event_id.to_string(),
            is_encrypted: false,
            mirrored_wrap_kind: None,
            client_pmis: None,
            payment_interaction: Some(PaymentInteractionMode::ExplicitGating),
            cancel: CancellationToken::new(),
        }
    }

    fn request_message(id: &str, method: &str, params: serde_json::Value) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!(id),
            method: method.to_string(),
            params: Some(params),
        })
    }

    fn call_message() -> JsonRpcMessage {
        request_message("client-req-1", "tools/call", json!({ "name": "echo" }))
    }

    fn identity_for(method: &str, params: &serde_json::Value) -> CanonicalInvocationIdentity {
        compute_canonical_invocation_identity(&client_pk(), method, Some(params))
            .expect("test params canonicalize")
    }

    fn call_identity() -> CanonicalInvocationIdentity {
        identity_for("tools/call", &json!({ "name": "echo" }))
    }

    /// Poll `cond` for up to two seconds (the detached verify runs on its own task).
    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..400 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not reached within 2 s: {what}");
    }

    // ── the gates ───────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn first_priced_request_gets_32042_and_drops() {
        let fx = fixture(
            GateProcessor::new("pmi-a")
                .with_ttl(600)
                .with_plan(VerifyPlan::Gated),
        );
        fx.routes
            .register(
                "evt-1".to_string(),
                client_pk(),
                json!("client-req-1"),
                None,
            )
            .await;
        fx.dispatch(test_ctx("evt-1"), call_message()).await;

        let responses = fx.sent.responses();
        assert_eq!(responses.len(), 1, "exactly one targeted response");
        // The WHOLE wire object: original inner id, code, message, instructions bytes,
        // exactly one option with absent fields omitted.
        assert_eq!(
            serde_json::to_value(&responses[0]).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": "client-req-1",
                "error": {
                    "code": -32042,
                    "message": "Payment Required",
                    "data": {
                        "instructions": PAYMENT_REQUIRED_INSTRUCTIONS,
                        "payment_options": [{
                            "amount": 21,
                            "pmi": "pmi-a",
                            "pay_req": "invoice-evt-1-1",
                            "ttl": 600
                        }]
                    }
                }
            })
        );
        assert!(fx.delivered().is_empty(), "the gated request is dropped");
        assert!(
            !fx.routes.has_event_route("evt-1").await,
            "the drop-cleanup releases the route"
        );
        wait_until("verify started", || {
            fx.verify_calls.load(Ordering::SeqCst) == 1
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transparent_session_forwards_untouched() {
        let fx = fixture(GateProcessor::new("pmi-a"));
        let mut ctx = test_ctx("evt-2");
        ctx.payment_interaction = Some(PaymentInteractionMode::Transparent);
        fx.dispatch(ctx, call_message()).await;
        assert_eq!(fx.delivered().len(), 1);
        assert!(fx.sent.responses().is_empty());
        assert_eq!(fx.store.get_pending_remaining_ms(&call_identity()), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn absent_mode_forwards() {
        // Reachable only from a hand-built context (the seam always resolves the
        // mode), but one `!=` away from gating every un-negotiated session.
        let fx = fixture(GateProcessor::new("pmi-a"));
        let mut ctx = test_ctx("evt-2b");
        ctx.payment_interaction = None;
        fx.dispatch(ctx, call_message()).await;
        assert_eq!(fx.delivered().len(), 1);
        assert!(fx.sent.responses().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unpriced_request_forwards() {
        let fx = fixture(GateProcessor::new("pmi-a"));
        fx.dispatch(
            test_ctx("evt-3"),
            request_message("free-1", "tools/call", json!({ "name": "free-tool" })),
        )
        .await;
        assert_eq!(fx.delivered().len(), 1);
        assert!(fx.sent.responses().is_empty());
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notification_forwards() {
        let fx = fixture(GateProcessor::new("pmi-a"));
        fx.dispatch(
            test_ctx("evt-3b"),
            JsonRpcMessage::Notification(JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "notifications/progress".to_string(),
                params: None,
            }),
        )
        .await;
        assert_eq!(fx.delivered().len(), 1);
        assert!(fx.sent.responses().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whole_method_pricing_gates_every_name() {
        // A pricing entry with no capability name prices every invocation of the
        // method, whatever the tool name.
        let fx = build_fixture(
            GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated),
            |mut o| {
                o.priced_capabilities = vec![priced("tools/call", None, 9)];
                o
            },
            &[],
        );
        fx.dispatch(
            test_ctx("evt-3c"),
            request_message("any-1", "tools/call", json!({ "name": "whatever" })),
        )
        .await;
        assert_eq!(fx.sent.codes(), vec![PAYMENT_REQUIRED_ERROR_CODE]);
        assert!(fx.delivered().is_empty());
    }

    // ── pending and claiming ────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn second_request_while_pending_gets_32043() {
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated));
        fx.dispatch(test_ctx("evt-4a"), call_message()).await;
        let before = fx.store.get_pending_remaining_ms(&call_identity());
        assert!(before > 0, "the first request parked a pending entry");

        fx.dispatch(test_ctx("evt-4b"), call_message()).await;
        let responses = fx.sent.responses();
        assert_eq!(fx.sent.codes(), vec![-32042, -32043]);
        // The whole -32043 object. The pending window was tightened to the grant
        // horizon (payment_ttl here, since the option carries no ttl), so ~300 s
        // remain and the clamp answers 2.
        assert_eq!(
            serde_json::to_value(&responses[1]).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": "client-req-1",
                "error": {
                    "code": -32043,
                    "message": "Payment Pending",
                    "data": {
                        "instructions": PAYMENT_PENDING_INSTRUCTIONS,
                        "retry_after": 2
                    }
                }
            })
        );
        assert!(
            fx.store.get_pending_remaining_ms(&call_identity()) > 0,
            "the duplicate must not disturb the original pending"
        );
        assert!(fx.delivered().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_tightened_pending_window_shows_in_retry_after() {
        // The option's own ttl (1 s) is far below payment_ttl (300 s): the pending
        // entry is retimed to ~1000 ms before the offer publishes, so the duplicate's
        // retry_after is 1, not the untightened 2.
        let fx = fixture(
            GateProcessor::new("pmi-a")
                .with_ttl(1)
                .with_plan(VerifyPlan::Gated),
        );
        fx.dispatch(test_ctx("evt-4c"), call_message()).await;
        fx.dispatch(test_ctx("evt-4d"), call_message()).await;
        let responses = fx.sent.responses();
        assert_eq!(responses[1].error.code, PAYMENT_PENDING_ERROR_CODE);
        let retry_after = responses[1]
            .error
            .data
            .as_ref()
            .and_then(|d| d.get("retry_after"))
            .and_then(|v| v.as_u64())
            .expect("retry_after present");
        assert_eq!(
            retry_after, 1,
            "retry_after must reflect the tightened (1 s) pending window"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn granted_identity_forwards_once() {
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated));
        fx.store.grant(&call_identity(), 10_000);

        fx.dispatch(test_ctx("evt-5a"), call_message()).await;
        assert_eq!(fx.delivered().len(), 1, "the claim forwards");
        assert!(fx.sent.responses().is_empty(), "a claim answers nothing");

        // Single-use: the next identical invocation starts a fresh offer cycle.
        fx.dispatch(test_ctx("evt-5b"), call_message()).await;
        assert_eq!(fx.sent.codes(), vec![PAYMENT_REQUIRED_ERROR_CODE]);
        assert_eq!(fx.delivered().len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn regenerated_progress_token_still_claims() {
        // `_meta` is stripped from the identity, so a retry whose progressToken was
        // regenerated matches the paid authorization.
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated));
        let paid_params = json!({ "name": "echo", "_meta": { "progressToken": 1 } });
        fx.store
            .grant(&identity_for("tools/call", &paid_params), 10_000);

        fx.dispatch(
            test_ctx("evt-6"),
            request_message(
                "client-req-2",
                "tools/call",
                json!({ "name": "echo", "_meta": { "progressToken": 2 } }),
            ),
        )
        .await;
        assert_eq!(fx.delivered().len(), 1);
        assert!(fx.sent.responses().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn identity_isolation() {
        let gated = || GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated);
        let wildcard_pricing = |mut o: ServerPaymentsOptions| {
            o.priced_capabilities = vec![
                priced("tools/call", None, 21),
                priced("prompts/get", None, 21),
            ];
            o
        };
        let grant_id = call_identity();

        // Different params: re-offers, and the original grant survives the mismatch.
        let fx = build_fixture(gated(), wildcard_pricing, &[]);
        fx.store.grant(&grant_id, 10_000);
        fx.dispatch(
            test_ctx("evt-6b1"),
            request_message("client-req-1", "tools/call", json!({ "name": "other" })),
        )
        .await;
        assert_eq!(fx.sent.codes(), vec![PAYMENT_REQUIRED_ERROR_CODE]);
        assert!(
            fx.store.claim(&grant_id),
            "a mismatched-params attempt must not consume the grant"
        );

        // Different client: re-offers, grant survives.
        let fx = build_fixture(gated(), wildcard_pricing, &[]);
        fx.store.grant(&grant_id, 10_000);
        let mut ctx = test_ctx("evt-6b2");
        ctx.client_pubkey = "d".repeat(64);
        fx.dispatch(ctx, call_message()).await;
        assert_eq!(fx.sent.codes(), vec![PAYMENT_REQUIRED_ERROR_CODE]);
        assert!(
            fx.store.claim(&grant_id),
            "another client's attempt must not consume the grant"
        );

        // Different method: re-offers, grant survives.
        let fx = build_fixture(gated(), wildcard_pricing, &[]);
        fx.store.grant(&grant_id, 10_000);
        fx.dispatch(
            test_ctx("evt-6b3"),
            request_message("client-req-1", "prompts/get", json!({ "name": "echo" })),
        )
        .await;
        assert_eq!(fx.sent.codes(), vec![PAYMENT_REQUIRED_ERROR_CODE]);
        assert!(
            fx.store.claim(&grant_id),
            "another method's attempt must not consume the grant"
        );

        // Same method and params under a different JSON-RPC id: claims.
        let fx = build_fixture(gated(), wildcard_pricing, &[]);
        fx.store.grant(&grant_id, 10_000);
        fx.dispatch(
            test_ctx("evt-6b4"),
            request_message("a-fresh-id", "tools/call", json!({ "name": "echo" })),
        )
        .await;
        assert_eq!(
            fx.delivered().len(),
            1,
            "the id is not part of the identity"
        );

        // Two DIFFERENT identities from ONE client, both mid-cycle: independent
        // pending state, each gets its own offer, neither parks the other.
        let fx = build_fixture(gated(), wildcard_pricing, &[]);
        fx.dispatch(test_ctx("evt-6b5"), call_message()).await;
        fx.dispatch(
            test_ctx("evt-6b6"),
            request_message("client-req-1", "tools/call", json!({ "name": "other" })),
        )
        .await;
        assert_eq!(
            fx.sent.codes(),
            vec![PAYMENT_REQUIRED_ERROR_CODE, PAYMENT_REQUIRED_ERROR_CODE],
            "distinct identities must not share pending state"
        );
        assert!(fx.store.get_pending_remaining_ms(&call_identity()) > 0);
        assert!(
            fx.store
                .get_pending_remaining_ms(&identity_for("tools/call", &json!({ "name": "other" })))
                > 0
        );
    }

    // ── the resolver arms ───────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn waive_clears_pending_and_forwards() {
        let fx = build_fixture(
            GateProcessor::new("pmi-a"),
            |o| {
                o.with_resolve_price(Arc::new(StaticResolver(ResolvePriceResult::Waive {
                    meta: None,
                })))
            },
            &[],
        );
        fx.dispatch(test_ctx("evt-7a"), call_message()).await;
        assert_eq!(fx.delivered().len(), 1, "a waived request forwards");
        assert!(fx.sent.responses().is_empty(), "a waiver answers nothing");
        assert_eq!(
            fx.store.get_pending_remaining_ms(&call_identity()),
            0,
            "the waiver must release the pending slot"
        );
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reject_sends_32000_clears_and_drops() {
        let fx = build_fixture(
            GateProcessor::new("pmi-a"),
            |o| {
                o.with_resolve_price(Arc::new(StaticResolver(ResolvePriceResult::Reject {
                    message: Some("not today".to_string()),
                })))
            },
            &[],
        );
        fx.dispatch(test_ctx("evt-7b"), call_message()).await;
        let responses = fx.sent.responses();
        assert_eq!(responses.len(), 1);
        // The whole object: the policy message, and NO `data` key at all.
        assert_eq!(
            serde_json::to_value(&responses[0]).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": "client-req-1",
                "error": { "code": -32000, "message": "not today" }
            })
        );
        assert!(fx.delivered().is_empty(), "a rejected request is dropped");
        assert_eq!(
            fx.store.get_pending_remaining_ms(&call_identity()),
            0,
            "the rejection must release the pending slot"
        );

        // A reasonless rejection falls back to the policy message.
        let fx = build_fixture(
            GateProcessor::new("pmi-a"),
            |o| {
                o.with_resolve_price(Arc::new(StaticResolver(ResolvePriceResult::Reject {
                    message: None,
                })))
            },
            &[],
        );
        fx.dispatch(test_ctx("evt-7c"), call_message()).await;
        assert_eq!(
            fx.sent.responses()[0].error.message,
            "Payment rejected by policy"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_processors_drops_with_no_offer() {
        // The only path that could ever yield zero payment options exits before the
        // offer is built: nothing at all reaches the wire.
        let sent = Sent::default();
        let attempts = Arc::new(AtomicUsize::new(0));
        let store = AuthorizationStore::new();
        let middleware = create_explicit_gating_middleware(ExplicitGatingMiddlewareParams::new(
            ServerPaymentsOptions::new(Vec::new(), vec![priced("tools/call", Some("echo"), 21)]),
            test_sender(sent.clone(), Arc::clone(&attempts), &[]),
            store.clone(),
        ));
        let (tx, rx) = mpsc::unbounded_channel();
        let fx = Fixture {
            middleware,
            routes: ServerEventRouteStore::new(),
            open_stream: ServerOpenStreamState::new(&OpenStreamConfig::default(), 10),
            tx,
            rx: StdMutex::new(rx),
            sent,
            attempts,
            store,
            create_calls: Arc::new(AtomicUsize::new(0)),
            verify_calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Semaphore::new(0)),
            stashed_cancels: Arc::new(StdMutex::new(Vec::new())),
        };

        fx.dispatch(test_ctx("evt-8"), call_message()).await;
        assert_eq!(
            fx.attempts.load(Ordering::SeqCst),
            0,
            "no publish may even be attempted without a processor"
        );
        assert!(fx.delivered().is_empty());
        assert_eq!(
            fx.store.get_pending_remaining_ms(&call_identity()),
            0,
            "the failure must release the pending slot"
        );
    }

    // ── publish-failure arms ────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn failed_32042_publish_clears_pending_and_spawns_no_verify() {
        let fx = build_fixture(
            GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated),
            |o| o,
            &[PAYMENT_REQUIRED_ERROR_CODE],
        );
        fx.dispatch(test_ctx("evt-9a"), call_message()).await;
        assert_eq!(
            fx.attempts.load(Ordering::SeqCst),
            1,
            "the publish was tried"
        );
        assert!(fx.sent.responses().is_empty(), "nothing reached the wire");
        assert_eq!(
            fx.store.get_pending_remaining_ms(&call_identity()),
            0,
            "a failed offer must not dead-block the identity"
        );
        // Give an erroneously spawned verify a chance to show itself.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            fx.verify_calls.load(Ordering::SeqCst),
            0,
            "no invoice reached the client, so nothing may be verified"
        );

        // The next identical invocation starts a fresh offer cycle immediately.
        fx.dispatch(test_ctx("evt-9b"), call_message()).await;
        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            2,
            "the retry mints a fresh offer rather than seeing a stale pending"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_32043_publish_leaves_pending() {
        // Only the duplicate's answer fails; the original offer cycle stays live and
        // its verification must still be able to grant.
        let fx = build_fixture(
            GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated),
            |o| o,
            &[PAYMENT_PENDING_ERROR_CODE],
        );
        fx.dispatch(test_ctx("evt-10a"), call_message()).await;
        assert_eq!(fx.sent.codes(), vec![PAYMENT_REQUIRED_ERROR_CODE]);

        fx.dispatch(test_ctx("evt-10b"), call_message()).await;
        assert_eq!(fx.attempts.load(Ordering::SeqCst), 2);
        assert!(
            fx.store.get_pending_remaining_ms(&call_identity()) > 0,
            "the failed duplicate answer must not clear the live cycle's pending"
        );

        // The original cycle settles and grants; the next retry claims and forwards.
        fx.release.add_permits(1);
        wait_until("verify completed", || {
            fx.stashed_cancels
                .lock()
                .unwrap()
                .first()
                .is_some_and(|token| token.is_cancelled())
        })
        .await;
        fx.dispatch(test_ctx("evt-10c"), call_message()).await;
        assert_eq!(
            fx.delivered().len(),
            1,
            "the paid retry claims and forwards"
        );
    }

    // ── the detached verify ─────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn verify_failure_clears_pending_and_reoffers_with_new_invoice() {
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::InstantErr));
        fx.dispatch(test_ctx("evt-11a"), call_message()).await;
        wait_until("pending cleared after verify failure", || {
            fx.store.get_pending_remaining_ms(&call_identity()) == 0
        })
        .await;

        fx.dispatch(test_ctx("evt-11b"), call_message()).await;
        assert_eq!(
            fx.sent.codes(),
            vec![-32042, -32042],
            "a fresh offer, not a park"
        );
        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            2,
            "the fresh offer carries a NEW invoice"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panicking_processor_clears_pending_and_never_grants() {
        // A crashing processor is a verification FAILURE: the caught panic must clear
        // the pending state and mint nothing. The money stake is the inverse: if a
        // panic ever granted, every processor crash would hand out a free execution.
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::Panic));
        fx.dispatch(test_ctx("evt-20a"), call_message()).await;
        assert_eq!(
            fx.sent.codes(),
            vec![PAYMENT_REQUIRED_ERROR_CODE],
            "the offer went out before the crash"
        );
        wait_until("pending cleared after the panic", || {
            fx.store.get_pending_remaining_ms(&call_identity()) == 0
        })
        .await;
        assert!(
            !fx.store.claim(&call_identity()),
            "a panicking verification must never grant"
        );

        // The next matching invocation starts a fresh offer cycle, never a claim.
        fx.dispatch(test_ctx("evt-20b"), call_message()).await;
        assert_eq!(fx.sent.codes(), vec![-32042, -32042]);
        assert!(fx.delivered().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verify_timeout_clears_pending_and_reoffers() {
        // Real time throughout (the store's clock is std::time::Instant). The polling
        // bound is min(verify_timeout, payment_ttl): the processor's default verify
        // window is 300 s, so only the 50 ms payment_ttl can explain a fast timeout.
        let fx = build_fixture(
            GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated),
            |o| o.with_payment_ttl(Duration::from_millis(50)),
            &[],
        );
        fx.dispatch(test_ctx("evt-12a"), call_message()).await;
        wait_until("the timeout fired and cancelled the verify child", || {
            fx.stashed_cancels
                .lock()
                .unwrap()
                .first()
                .is_some_and(|token| token.is_cancelled())
        })
        .await;
        assert_eq!(fx.store.get_pending_remaining_ms(&call_identity()), 0);

        fx.dispatch(test_ctx("evt-12b"), call_message()).await;
        assert_eq!(fx.sent.codes(), vec![-32042, -32042]);
        assert_eq!(
            fx.create_calls.load(Ordering::SeqCst),
            2,
            "the post-timeout retry mints a fresh invoice"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completed_verify_leaves_ctx_cancel_alive() {
        let fx = fixture(GateProcessor::new("pmi-a"));
        let ctx = test_ctx("evt-13");
        let event_token = ctx.cancel.clone();
        fx.dispatch(ctx, call_message()).await;

        // The completion-path cancel (the `finally` analogue) MUST fire on the child...
        wait_until("the verify child token was cancelled on completion", || {
            fx.stashed_cancels
                .lock()
                .unwrap()
                .first()
                .is_some_and(|token| token.is_cancelled())
        })
        .await;
        // ...and the per-event token must survive: it is shared by every middleware on
        // the event and doubles as the transport-shutdown cascade.
        assert!(
            !event_token.is_cancelled(),
            "a completed verify must never cancel the per-event token"
        );

        // And the completion granted: the retry claims and forwards.
        fx.dispatch(test_ctx("evt-13x"), call_message()).await;
        assert_eq!(fx.delivered().len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_ctx_clears_pending() {
        // close() semantics at unit scale: cancelling the per-event token mid-verify
        // reaches the processor through the child, the conforming fake answers with an
        // error, and the pending state is released.
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated));
        let ctx = test_ctx("evt-13b");
        let event_token = ctx.cancel.clone();
        fx.dispatch(ctx, call_message()).await;
        wait_until("verify started", || {
            fx.verify_calls.load(Ordering::SeqCst) == 1
        })
        .await;

        event_token.cancel();
        wait_until("pending cleared after cancellation", || {
            fx.store.get_pending_remaining_ms(&call_identity()) == 0
        })
        .await;

        // No grant was minted: the next invocation re-offers.
        fx.dispatch(test_ctx("evt-13c"), call_message()).await;
        assert_eq!(fx.sent.codes(), vec![-32042, -32042]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_identical_requests_yield_one_offer() {
        // Store empty: the composed op serializes two genuinely concurrent identical
        // invocations into exactly one offer and one park.
        let fx = Arc::new(fixture(
            GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated),
        ));
        let a = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move { fx.dispatch(test_ctx("evt-14a"), call_message()).await })
        };
        let b = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move { fx.dispatch(test_ctx("evt-14b"), call_message()).await })
        };
        a.await.unwrap();
        b.await.unwrap();
        let mut codes = fx.sent.codes();
        codes.sort_unstable();
        assert_eq!(
            codes,
            vec![-32043, -32042],
            "one offer, one park, no double invoice"
        );
        assert!(fx.delivered().is_empty());

        // With a grant present: exactly one forward, and the loser starts a fresh
        // offer cycle (a new payment for a new execution).
        let fx = Arc::new(fixture(
            GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated),
        ));
        fx.store.grant(&call_identity(), 10_000);
        let a = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move { fx.dispatch(test_ctx("evt-14c"), call_message()).await })
        };
        let b = {
            let fx = Arc::clone(&fx);
            tokio::spawn(async move { fx.dispatch(test_ctx("evt-14d"), call_message()).await })
        };
        a.await.unwrap();
        b.await.unwrap();
        assert_eq!(fx.delivered().len(), 1, "one grant, one execution");
        assert_eq!(fx.sent.codes(), vec![PAYMENT_REQUIRED_ERROR_CODE]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forwarded_message_is_byte_identical_including_meta() {
        // The identity strip happens on a copy; the forwarded message is the original,
        // `_meta` and all, byte for byte.
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated));
        let params = json!({ "name": "echo", "_meta": { "progressToken": "tok-7" } });
        fx.store.grant(&identity_for("tools/call", &params), 10_000);

        let original = request_message("client-req-9", "tools/call", params);
        let original_bytes = serde_json::to_value(&original).unwrap();
        fx.dispatch(test_ctx("evt-15"), original).await;

        let delivered = fx.delivered();
        assert_eq!(delivered.len(), 1);
        assert_eq!(
            serde_json::to_value(&delivered[0].message).unwrap(),
            original_bytes,
            "the forwarded message must be byte-identical, `_meta` included"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_grant_reoffers() {
        // Expired-direction assertion: tiny TTL plus a real sleep (a sleep only
        // oversleeps).
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated));
        fx.store.grant(&call_identity(), 20);
        std::thread::sleep(Duration::from_millis(100));

        fx.dispatch(test_ctx("evt-16"), call_message()).await;
        assert_eq!(
            fx.sent.codes(),
            vec![PAYMENT_REQUIRED_ERROR_CODE],
            "an expired grant is not claimable; the invocation re-offers"
        );
        assert!(fx.delivered().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn grant_expires_at_the_options_horizon() {
        // The option's ttl (1 s) is far below payment_ttl (300 s): the grant minted at
        // verify success must expire at the OPTION's horizon. Real time; the 200 ms
        // margin past the horizon is on the expired (flake-free) direction.
        let fx = fixture(GateProcessor::new("pmi-a").with_ttl(1));
        fx.dispatch(test_ctx("evt-16b"), call_message()).await;
        wait_until("verify completed", || {
            fx.stashed_cancels
                .lock()
                .unwrap()
                .first()
                .is_some_and(|token| token.is_cancelled())
        })
        .await;

        tokio::time::sleep(Duration::from_millis(1200)).await;
        fx.dispatch(test_ctx("evt-16b2"), call_message()).await;
        assert_eq!(
            fx.sent.codes(),
            vec![-32042, -32042],
            "the grant must expire at the option's 1 s horizon, not payment_ttl"
        );
        assert!(fx.delivered().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn zero_ttl_option_grants_the_fallback_horizon() {
        // A `ttl: 0` option must NOT mint a grant that expires at birth (paid but
        // stranded): the zero is treated as absent and the grant lives for
        // payment_ttl. Live-read direction, so the fallback horizon (300 s) is far
        // above the flake floor.
        let fx = fixture(GateProcessor::new("pmi-a").with_ttl(0));
        fx.dispatch(test_ctx("evt-16c"), call_message()).await;
        wait_until("verify completed", || {
            fx.stashed_cancels
                .lock()
                .unwrap()
                .first()
                .is_some_and(|token| token.is_cancelled())
        })
        .await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        fx.dispatch(test_ctx("evt-16c2"), call_message()).await;
        assert_eq!(
            fx.delivered().len(),
            1,
            "the paid grant must be claimable after birth"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canonicalize_failure_drops() {
        // An integer outside the JSON safe range cannot be canonicalized; the request
        // cannot be gated, so it is dropped (fail closed), with nothing on the wire.
        let fx = fixture(GateProcessor::new("pmi-a"));
        fx.dispatch(
            test_ctx("evt-17"),
            request_message(
                "client-req-1",
                "tools/call",
                json!({ "name": "echo", "big": 9_007_199_254_740_993u64 }),
            ),
        )
        .await;
        assert!(fx.delivered().is_empty(), "fail closed: never forward");
        assert_eq!(fx.attempts.load(Ordering::SeqCst), 0, "nothing on the wire");
        assert_eq!(fx.create_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn retry_after_arithmetic() {
        // The three-point table: raced-to-zero clamps up to 1, mid-band rounds up,
        // and a long window clamps down to 2.
        assert_eq!(retry_after_secs(0), 1);
        assert_eq!(retry_after_secs(1500), 2);
        assert_eq!(retry_after_secs(300_000), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verify_grant_claim_forward_seam() {
        // The full seam at unit level: offer, park, settle, claim, single-use.
        let fx = fixture(GateProcessor::new("pmi-a").with_plan(VerifyPlan::Gated));

        // (1) The first invocation offers and starts exactly one verification.
        fx.dispatch(test_ctx("evt-19a"), call_message()).await;
        assert_eq!(fx.sent.codes(), vec![PAYMENT_REQUIRED_ERROR_CODE]);
        wait_until("verify started", || {
            fx.verify_calls.load(Ordering::SeqCst) == 1
        })
        .await;

        // (2) A retry mid-verify parks.
        fx.dispatch(test_ctx("evt-19b"), call_message()).await;
        assert_eq!(fx.sent.codes(), vec![-32042, -32043]);
        assert_eq!(
            fx.verify_calls.load(Ordering::SeqCst),
            1,
            "the park must not start a second verification"
        );

        // (3) Releasing the gate settles the payment and grants.
        fx.release.add_permits(1);
        wait_until("verify completed", || {
            fx.stashed_cancels
                .lock()
                .unwrap()
                .first()
                .is_some_and(|token| token.is_cancelled())
        })
        .await;

        // (4) The next retry claims and forwards, answering nothing.
        fx.dispatch(test_ctx("evt-19c"), call_message()).await;
        assert_eq!(fx.delivered().len(), 1);
        assert_eq!(fx.sent.codes().len(), 2, "a claim adds no response");

        // (5) The grant was single-use: one more retry re-offers.
        fx.dispatch(test_ctx("evt-19d"), call_message()).await;
        assert_eq!(fx.sent.codes(), vec![-32042, -32043, -32042]);
    }
}
