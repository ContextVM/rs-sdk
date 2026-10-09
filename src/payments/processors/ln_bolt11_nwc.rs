//! CEP-8 payment processor for `bitcoin-lightning-bolt11`, backed by NIP-47 (NWC).
//!
//! Server side of the first Phase B rail: `create_payment_required` mints a
//! BOLT11 invoice with `make_invoice`, and `verify_payment` waits for
//! settlement either by polling `lookup_invoice` or by listening for a
//! `payment_received` notification.
//!
//! # Bounds are this rail's job
//!
//! `create_payment_required` runs in the middleware's unbounded,
//! cancellation-blind initiation phase, so the only thing stopping a hung
//! wallet from wedging it is the client's own `response_timeout`.
//! `verify_payment` runs under a `CancellationToken` the middleware owns, and
//! every wait here selects on it: a poller that ignored it would outlive the
//! payment TTL and keep hitting the wallet long after the request was
//! abandoned.
//!
//! # A cancelled verify is a failure, never a settlement
//!
//! Every cancellation path returns `Err`. Returning `Ok(VerifyOutcome::default())`
//! would be read by the middleware as "paid", forwarding an unpaid invocation.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::future::{BoxFuture, Shared};
use futures::FutureExt;
use lru::LruCache;
use tokio::sync::{oneshot, Mutex, OnceCell};

use crate::payments::constants::PMI_BITCOIN_LIGHTNING_BOLT11;
use crate::payments::errors::PaymentError;
use crate::payments::nip47::client::{NwcClient, NwcClientOptions};
use crate::payments::nip47::error::NwcError;
use crate::payments::nip47::types::{
    sats_to_msats, NwcInvoiceResult, NOTIFICATION_PAYMENT_RECEIVED,
};
use crate::payments::nip47::uri::{parse_nwc_uri, NwcConnection};
use crate::payments::traits::PaymentProcessor;
use crate::payments::types::{
    Meta, PaymentProcessorCreateParams, PaymentProcessorVerifyParams, PaymentRequiredParams,
    VerifyOutcome,
};
use crate::relay::RelayPoolTrait;

const LOG_TARGET: &str = "contextvm::payments::nwc_processor";

/// NIP-47 error code meaning "the wallet does not know this invoice yet".
const ERROR_CODE_NOT_FOUND: &str = "NOT_FOUND";

/// ts `computeNextDelayMs` schedule, in milliseconds. Fast early checks, since
/// most invoices settle quickly, then back off.
const BACKOFF_SCHEDULE_MS: [u64; 9] = [500, 750, 1000, 1500, 2500, 4000, 6500, 10_000, 15_000];

/// Upper bound on the jitter added to each poll delay, in milliseconds.
const JITTER_MAX_MS: u64 = 250;

/// Default fallback TTL for an issued invoice.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);
/// Default `lookup_invoice` poll interval floor.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1500);
/// Default per-request wallet response timeout.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
/// Default cap on concurrently deduplicated verifications.
pub const DEFAULT_MAX_IN_FLIGHT_VERIFICATIONS: usize = 5000;
/// Default cap on cached invoice to payment-hash mappings.
pub const DEFAULT_INVOICE_HASH_CACHE_SIZE: usize = 10_000;

/// Construction options for [`LnBolt11NwcPaymentProcessor`].
///
/// Defaults mirror ts `LnBolt11NwcPaymentProcessorOptions`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LnBolt11NwcPaymentProcessorOptions {
    /// TTL advertised on `payment_required`. The wallet's own `expires_at` is
    /// deliberately ignored: providers return non-standard values, and ts keeps
    /// this predictable. Default 300 s.
    pub ttl: Duration,
    /// `make_invoice.expiry`. Defaults to [`Self::ttl`] when `None`.
    pub invoice_expiry: Option<Duration>,
    /// Floor under the `lookup_invoice` backoff schedule. Default 1500 ms.
    pub poll_interval: Duration,
    /// Per-request wallet response timeout. Default 60 s.
    pub response_timeout: Duration,
    /// `Some(false)` always polls, `Some(true)` always uses notifications,
    /// `None` auto-detects once from the wallet's info event. Default `None`.
    pub notification_verification: Option<bool>,
    /// Cap on concurrently deduplicated verifications. Default 5000.
    pub max_in_flight_verifications: usize,
    /// Cap on cached invoice to payment-hash mappings. Default 10000.
    pub invoice_hash_cache_size: usize,
}

impl Default for LnBolt11NwcPaymentProcessorOptions {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_TTL,
            invoice_expiry: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
            notification_verification: None,
            max_in_flight_verifications: DEFAULT_MAX_IN_FLIGHT_VERIFICATIONS,
            invoice_hash_cache_size: DEFAULT_INVOICE_HASH_CACHE_SIZE,
        }
    }
}

impl LnBolt11NwcPaymentProcessorOptions {
    /// Options with the ts defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the TTL advertised on `payment_required`.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Set `make_invoice.expiry`, which otherwise follows the TTL.
    pub fn with_invoice_expiry(mut self, expiry: Duration) -> Self {
        self.invoice_expiry = Some(expiry);
        self
    }

    /// Set the floor under the `lookup_invoice` backoff schedule.
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Set the per-request wallet response timeout.
    pub fn with_response_timeout(mut self, response_timeout: Duration) -> Self {
        self.response_timeout = response_timeout;
        self
    }

    /// Force notification-mode verification on or off, skipping auto-detection.
    pub fn with_notification_verification(mut self, enabled: bool) -> Self {
        self.notification_verification = Some(enabled);
        self
    }

    /// Set the cap on concurrently deduplicated verifications.
    pub fn with_max_in_flight_verifications(mut self, max: usize) -> Self {
        self.max_in_flight_verifications = max;
        self
    }

    /// Set the cap on cached invoice to payment-hash mappings.
    pub fn with_invoice_hash_cache_size(mut self, size: usize) -> Self {
        self.invoice_hash_cache_size = size;
        self
    }
}

/// A deduplicated verification. `PaymentError` is not `Clone`, so the shared
/// output is an `Arc` and joiners re-wrap the error by message.
type VerifyInFlight = Shared<BoxFuture<'static, Arc<Result<VerifyOutcome, PaymentError>>>>;

struct Inner {
    client: Arc<NwcClient>,
    options: LnBolt11NwcPaymentProcessorOptions,
    in_flight: Mutex<LruCache<String, VerifyInFlight>>,
    invoice_hash: Mutex<LruCache<String, String>>,
    /// Resolved once: whether the wallet supports `payment_received`.
    notifications_enabled: OnceCell<bool>,
    /// Resolved once: the notification subscription has been installed.
    notifications_subscribed: OnceCell<()>,
    /// payment_hash to the verifications waiting on it.
    waiters: Arc<Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>>,
}

/// CEP-8 processor for `bitcoin-lightning-bolt11` over NIP-47.
pub struct LnBolt11NwcPaymentProcessor {
    inner: Arc<Inner>,
}

impl LnBolt11NwcPaymentProcessor {
    /// Build from a NWC connection string and a relay pool.
    ///
    /// # Errors
    ///
    /// Returns [`PaymentError::Processor`] when the connection string does not parse.
    pub fn from_uri(
        pool: Arc<dyn RelayPoolTrait>,
        uri: &str,
        options: LnBolt11NwcPaymentProcessorOptions,
    ) -> Result<Self, PaymentError> {
        let connection = parse_nwc_uri(uri).map_err(NwcError::into_processor_error)?;
        Self::from_connection(pool, connection, options)
    }

    /// Build from an already-parsed connection.
    pub fn from_connection(
        pool: Arc<dyn RelayPoolTrait>,
        connection: NwcConnection,
        options: LnBolt11NwcPaymentProcessorOptions,
    ) -> Result<Self, PaymentError> {
        let client = NwcClient::with_pool(
            pool,
            connection,
            NwcClientOptions::new().with_response_timeout(options.response_timeout),
        )
        .map_err(NwcError::into_processor_error)?;
        Ok(Self::with_client(Arc::new(client), options))
    }

    /// Build around an already-constructed client, for tests and custom transports.
    pub fn with_client(
        client: Arc<NwcClient>,
        options: LnBolt11NwcPaymentProcessorOptions,
    ) -> Self {
        let in_flight_cap = NonZeroUsize::new(options.max_in_flight_verifications)
            .unwrap_or(NonZeroUsize::new(1).expect("1 is non-zero"));
        let hash_cap = NonZeroUsize::new(options.invoice_hash_cache_size)
            .unwrap_or(NonZeroUsize::new(1).expect("1 is non-zero"));

        let notifications_enabled = OnceCell::new();
        // An explicit setting short-circuits the info-event probe entirely.
        if let Some(explicit) = options.notification_verification {
            notifications_enabled
                .set(explicit)
                .expect("freshly constructed OnceCell is empty");
        }

        Self {
            inner: Arc::new(Inner {
                client,
                options,
                in_flight: Mutex::new(LruCache::new(in_flight_cap)),
                invoice_hash: Mutex::new(LruCache::new(hash_cap)),
                notifications_enabled,
                notifications_subscribed: OnceCell::new(),
                waiters: Arc::new(Mutex::new(HashMap::new())),
            }),
        }
    }

    /// The effective `make_invoice.expiry`.
    fn invoice_expiry(&self) -> Duration {
        self.inner
            .options
            .invoice_expiry
            .unwrap_or(self.inner.options.ttl)
    }
}

impl Inner {
    /// Whether to verify by notification, probing the wallet's info event at
    /// most once. A probe failure means polling, never a hard error.
    async fn notifications_enabled(&self) -> bool {
        *self
            .notifications_enabled
            .get_or_init(|| async {
                match self.client.fetch_info_notification_types().await {
                    Ok(types) => types.iter().any(|t| t == NOTIFICATION_PAYMENT_RECEIVED),
                    Err(e) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            error = %e,
                            "NWC info probe failed; falling back to polling"
                        );
                        false
                    }
                }
            })
            .await
    }

    /// Install the notification sink exactly once.
    ///
    /// Concurrent verifications racing here would otherwise each subscribe, and
    /// since `RelayPoolTrait` has no unsubscribe every extra sink would live
    /// for the process lifetime.
    async fn ensure_notifications_subscribed(&self) -> Result<(), PaymentError> {
        let waiters = Arc::clone(&self.waiters);
        let client = Arc::clone(&self.client);
        self.notifications_subscribed
            .get_or_try_init(|| async move {
                client
                    .on_notification(move |payload| {
                        if payload.notification_type != NOTIFICATION_PAYMENT_RECEIVED {
                            return;
                        }
                        let Some(hash) = payload.payment_hash() else {
                            return;
                        };
                        if hash.is_empty() {
                            return;
                        }
                        let hash = hash.to_string();
                        let waiters = Arc::clone(&waiters);
                        // The sink runs on the reader task and must not block,
                        // so the map touch is detached.
                        tokio::spawn(async move {
                            let taken = waiters.lock().await.remove(&hash);
                            for w in taken.unwrap_or_default() {
                                let _ = w.send(());
                            }
                        });
                    })
                    .await
            })
            .await?;
        Ok(())
    }

    async fn cached_payment_hash(&self, pay_req: &str) -> Option<String> {
        self.invoice_hash.lock().await.get(pay_req).cloned()
    }

    async fn cache_payment_hash(&self, pay_req: &str, hash: &str) {
        if hash.is_empty() {
            return;
        }
        self.invoice_hash
            .lock()
            .await
            .put(pay_req.to_string(), hash.to_string());
    }

    /// Delay before poll attempt `attempt`, floored at `poll_interval` and jittered.
    ///
    /// The jitter is derived from the clock rather than a RNG: this crate has no
    /// direct `rand` dependency, and de-stampeding shared wallet infrastructure
    /// does not need cryptographic randomness.
    fn next_delay(&self, attempt: usize) -> Duration {
        let idx = attempt.min(BACKOFF_SCHEDULE_MS.len() - 1);
        let base = Duration::from_millis(BACKOFF_SCHEDULE_MS[idx]);
        let floored = base.max(self.options.poll_interval);
        let jitter = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_nanos()) % JITTER_MAX_MS)
            .unwrap_or(0);
        floored + Duration::from_millis(jitter)
    }
}

/// The settlement receipt for a verified invoice.
///
/// Prefers `payment_hash`, a stable identifier that is not the preimage. Wallets
/// that omit it on lookup still get a stable correlation string, matching ts's
/// `settled:<request_event_id>[:<settled_at>]`.
fn receipt_for(result: &NwcInvoiceResult, request_event_id: &str) -> String {
    match result.payment_hash.as_deref() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => match result.settled_at {
            Some(t) if t != 0 => format!("settled:{request_event_id}:{t}"),
            _ => format!("settled:{request_event_id}"),
        },
    }
}

fn outcome_with_receipt(receipt: String) -> VerifyOutcome {
    let mut meta = Meta::new();
    meta.insert(
        "payment_hash".to_string(),
        serde_json::Value::String(receipt),
    );
    VerifyOutcome { meta: Some(meta) }
}

fn cancelled() -> PaymentError {
    PaymentError::Processor("verify_payment cancelled".to_string())
}

#[async_trait]
impl PaymentProcessor for LnBolt11NwcPaymentProcessor {
    fn pmi(&self) -> &str {
        PMI_BITCOIN_LIGHTNING_BOLT11
    }

    async fn create_payment_required(
        &self,
        params: PaymentProcessorCreateParams,
    ) -> Result<PaymentRequiredParams, PaymentError> {
        // Reject a bad amount before it can reach the wallet.
        let amount_msats = sats_to_msats(params.amount)?;

        let envelope = self
            .inner
            .client
            .make_invoice(
                amount_msats,
                params.description.as_deref(),
                Some(self.invoice_expiry()),
            )
            .await
            .map_err(NwcError::into_processor_error)?;

        if let Some(err) = envelope.error {
            return Err(NwcError::Wallet {
                method: "make_invoice".to_string(),
                detail: err.describe(),
            }
            .into_processor_error());
        }

        let result = envelope
            .result
            .ok_or_else(|| PaymentError::Processor("NWC make_invoice returned no result".into()))?;

        let invoice = match result.invoice.as_deref() {
            Some(i) if !i.is_empty() => i.to_string(),
            _ => {
                return Err(PaymentError::Processor(
                    "NWC make_invoice returned no invoice".into(),
                ))
            }
        };

        if let Some(hash) = result.payment_hash.as_deref() {
            self.inner.cache_payment_hash(&invoice, hash).await;
        }

        // Never log the invoice itself: it is a bearer payment request.
        tracing::debug!(
            target: LOG_TARGET,
            request_event_id = %params.request_event_id,
            amount_sats = params.amount,
            has_payment_hash = result.payment_hash.is_some(),
            "NWC invoice issued"
        );

        Ok(PaymentRequiredParams {
            amount: params.amount,
            pay_req: invoice,
            pmi: PMI_BITCOIN_LIGHTNING_BOLT11.to_string(),
            description: params.description,
            ttl: Some(self.inner.options.ttl.as_secs()),
            meta: None,
        })
    }

    async fn verify_payment(
        &self,
        params: PaymentProcessorVerifyParams,
    ) -> Result<VerifyOutcome, PaymentError> {
        // Join an existing verification for the same invoice rather than
        // doubling relay and wallet load under duplicate delivery.
        let shared = {
            let mut cache = self.inner.in_flight.lock().await;
            match cache.get(&params.pay_req) {
                Some(existing) => existing.clone(),
                None => {
                    let inner = Arc::clone(&self.inner);
                    let p = params.clone();
                    let key = params.pay_req.clone();
                    let fut: BoxFuture<'static, Arc<Result<VerifyOutcome, PaymentError>>> =
                        async move { Arc::new(verify_once(inner, p).await) }.boxed();
                    let shared = fut.shared();
                    cache.put(key, shared.clone());
                    shared
                }
            }
        };

        let result = shared.await;

        // Release the slot so a later retry of the same invoice re-runs rather
        // than replaying a stale terminal answer.
        self.inner.in_flight.lock().await.pop(&params.pay_req);

        match &*result {
            Ok(outcome) => Ok(outcome.clone()),
            // `PaymentError` is not `Clone`; joiners re-wrap by message.
            Err(e) => Err(PaymentError::Processor(e.to_string())),
        }
    }
}

/// One verification attempt, shared by every caller deduplicated onto it.
async fn verify_once(
    inner: Arc<Inner>,
    params: PaymentProcessorVerifyParams,
) -> Result<VerifyOutcome, PaymentError> {
    if params.cancel.is_cancelled() {
        return Err(cancelled());
    }

    if inner.notifications_enabled().await {
        match verify_by_notification(Arc::clone(&inner), &params).await {
            Ok(outcome) => return Ok(outcome),
            Err(NotificationUnavailable::Cancelled) => return Err(cancelled()),
            Err(NotificationUnavailable::Failed(e)) => return Err(e),
            // Divergence from ts, which throws here: fall back to polling when
            // no payment_hash is cached, rather than failing a payable invoice.
            Err(NotificationUnavailable::NoPaymentHash) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    request_event_id = %params.request_event_id,
                    "no cached payment_hash for notification mode; falling back to polling"
                );
            }
        }
    }

    verify_by_polling(inner, &params).await
}

/// Why notification-mode verification could not produce an answer.
enum NotificationUnavailable {
    /// No `payment_hash` is cached for this invoice, so no notification can be
    /// correlated to it.
    NoPaymentHash,
    /// The verification was cancelled while waiting.
    Cancelled,
    /// Subscribing failed.
    Failed(PaymentError),
}

async fn verify_by_notification(
    inner: Arc<Inner>,
    params: &PaymentProcessorVerifyParams,
) -> Result<VerifyOutcome, NotificationUnavailable> {
    if let Err(e) = inner.ensure_notifications_subscribed().await {
        return Err(NotificationUnavailable::Failed(e));
    }

    let Some(hash) = inner.cached_payment_hash(&params.pay_req).await else {
        return Err(NotificationUnavailable::NoPaymentHash);
    };

    let (tx, rx) = oneshot::channel();
    inner
        .waiters
        .lock()
        .await
        .entry(hash.clone())
        .or_default()
        .push(tx);

    tokio::select! {
        _ = params.cancel.cancelled() => {
            // Drop our waiter so a later notification cannot accumulate senders
            // for a verification nobody is awaiting.
            let mut guard = inner.waiters.lock().await;
            if let Some(list) = guard.get_mut(&hash) {
                list.retain(|w| !w.is_closed());
                if list.is_empty() {
                    guard.remove(&hash);
                }
            }
            Err(NotificationUnavailable::Cancelled)
        }
        received = rx => match received {
            Ok(()) => Ok(outcome_with_receipt(hash)),
            Err(_) => Err(NotificationUnavailable::Failed(PaymentError::Processor(
                "NWC notification waiter dropped".to_string(),
            ))),
        },
    }
}

async fn verify_by_polling(
    inner: Arc<Inner>,
    params: &PaymentProcessorVerifyParams,
) -> Result<VerifyOutcome, PaymentError> {
    let mut attempt = 0usize;

    loop {
        if params.cancel.is_cancelled() {
            return Err(cancelled());
        }

        let cached = inner.cached_payment_hash(&params.pay_req).await;

        // Race the wallet call against cancellation: without this, a hung
        // wallet keeps the poller alive past the payment TTL.
        let envelope = tokio::select! {
            _ = params.cancel.cancelled() => return Err(cancelled()),
            res = inner
                .client
                .lookup_invoice(cached.as_deref(), Some(&params.pay_req)) =>
                res.map_err(NwcError::into_processor_error)?,
        };

        match envelope.error {
            // A lagging wallet that has not seen the invoice yet is pending,
            // not failed. Every other code is fatal.
            Some(err) if err.code_upper() == ERROR_CODE_NOT_FOUND => {}
            Some(err) => {
                return Err(NwcError::Wallet {
                    method: "lookup_invoice".to_string(),
                    detail: err.describe(),
                }
                .into_processor_error())
            }
            None => {
                if let Some(result) = envelope.result {
                    if cached.is_none() {
                        if let Some(h) = result.payment_hash.as_deref() {
                            inner.cache_payment_hash(&params.pay_req, h).await;
                        }
                    }

                    tracing::debug!(
                        target: LOG_TARGET,
                        request_event_id = %params.request_event_id,
                        state = result.state.as_deref().unwrap_or("unknown"),
                        has_preimage = result.preimage.is_some(),
                        has_settled_at = result.settled_at.is_some(),
                        has_payment_hash = result.payment_hash.is_some(),
                        "NWC lookup_invoice result"
                    );

                    if result.is_settled() {
                        return Ok(outcome_with_receipt(receipt_for(
                            &result,
                            &params.request_event_id,
                        )));
                    }

                    if result.is_terminal_failure() {
                        let state = result.state.as_deref().unwrap_or("unknown");
                        return Err(PaymentError::Processor(format!("Invoice {state}")));
                    }
                }
            }
        }

        let delay = inner.next_delay(attempt);
        attempt = attempt.saturating_add(1);

        tokio::select! {
            _ = params.cancel.cancelled() => return Err(cancelled()),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::payments::nip47::mock_wallet::{MockWallet, WalletBehavior};
    use crate::relay::MockRelayPool;
    use nostr_sdk::prelude::*;
    use tokio_util::sync::CancellationToken;

    const SATS: i64 = 21;

    struct Harness {
        wallet: Arc<MockWallet>,
        processor: LnBolt11NwcPaymentProcessor,
    }

    fn opts() -> LnBolt11NwcPaymentProcessorOptions {
        LnBolt11NwcPaymentProcessorOptions {
            // Keep the polling tests fast; the schedule floor is what the
            // backoff test exercises directly.
            poll_interval: Duration::from_millis(20),
            response_timeout: Duration::from_millis(600),
            notification_verification: Some(false),
            ..Default::default()
        }
    }

    async fn harness_with(options: LnBolt11NwcPaymentProcessorOptions) -> Harness {
        let wallet_pool = Arc::new(MockRelayPool::new());
        let client_pool = Arc::new(wallet_pool.linked_with_keys(Keys::generate()));
        let wallet = Arc::new(MockWallet::new(Arc::clone(&wallet_pool)));
        wallet.start().await.expect("wallet starts");

        let secret = SecretKey::generate();
        let uri = format!(
            "nostr+walletconnect://{}?relay=wss%3A%2F%2Fmock.relay&secret={}",
            wallet.public_key().to_hex(),
            secret.to_secret_hex()
        );
        let connection = parse_nwc_uri(&uri).expect("uri parses");
        let response_timeout = options.response_timeout;
        let client = Arc::new(
            NwcClient::with_pool(
                Arc::clone(&client_pool) as Arc<dyn RelayPoolTrait>,
                connection,
                NwcClientOptions::new().with_response_timeout(response_timeout),
            )
            .expect("options valid"),
        );

        Harness {
            wallet,
            processor: LnBolt11NwcPaymentProcessor::with_client(client, options),
        }
    }

    async fn harness() -> Harness {
        harness_with(opts()).await
    }

    fn create_params() -> PaymentProcessorCreateParams {
        PaymentProcessorCreateParams {
            amount: SATS,
            description: Some("a priced tool call".to_string()),
            request_event_id: "evt-1".to_string(),
            client_pubkey: "client-1".to_string(),
        }
    }

    fn verify_params(pay_req: &str, cancel: CancellationToken) -> PaymentProcessorVerifyParams {
        PaymentProcessorVerifyParams {
            pay_req: pay_req.to_string(),
            request_event_id: "evt-1".to_string(),
            client_pubkey: "client-1".to_string(),
            cancel,
        }
    }

    fn invoice_answer(hash: Option<&str>) -> WalletBehavior {
        let mut v = serde_json::json!({ "invoice": "lnbc210n1p", "state": "pending" });
        if let Some(h) = hash {
            v["payment_hash"] = serde_json::Value::String(h.to_string());
        }
        WalletBehavior::Answer(v)
    }

    // ── create_payment_required ──────────────────────────────────────────────

    #[tokio::test]
    async fn issues_an_invoice_in_msats_with_the_configured_ttl() {
        let h = harness().await;
        h.wallet
            .set_behavior("make_invoice", invoice_answer(Some("hash-1")))
            .await;

        let out = h
            .processor
            .create_payment_required(create_params())
            .await
            .expect("invoice issued");

        assert_eq!(out.pay_req, "lnbc210n1p");
        assert_eq!(out.amount, SATS);
        assert_eq!(out.pmi, PMI_BITCOIN_LIGHTNING_BOLT11);
        // The wallet's own expires_at is ignored; the configured ttl wins.
        assert_eq!(out.ttl, Some(DEFAULT_TTL.as_secs()));

        let calls = h.wallet.calls().await;
        let (method, params) = &calls[0];
        assert_eq!(method, "make_invoice");
        // sats converted to msats, and the expiry carried.
        assert_eq!(params["amount"].as_u64(), Some(21_000));
        assert_eq!(params["expiry"].as_u64(), Some(DEFAULT_TTL.as_secs()));
    }

    #[tokio::test]
    async fn rejects_non_positive_and_overflowing_amounts_before_calling_the_wallet() {
        let h = harness().await;
        h.wallet
            .set_behavior("make_invoice", invoice_answer(None))
            .await;

        for bad in [0i64, -1, i64::MAX] {
            let mut p = create_params();
            p.amount = bad;
            assert!(
                h.processor.create_payment_required(p).await.is_err(),
                "amount {bad} must be rejected"
            );
        }
        assert_eq!(
            h.wallet.call_count("make_invoice").await,
            0,
            "a bad amount must never reach the wallet"
        );
    }

    #[tokio::test]
    async fn a_wallet_error_fails_invoice_creation() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Error {
                    code: "INSUFFICIENT_BALANCE".to_string(),
                    message: "no inbound".to_string(),
                },
            )
            .await;

        let err = h
            .processor
            .create_payment_required(create_params())
            .await
            .expect_err("wallet error must fail creation");
        assert!(err.to_string().contains("INSUFFICIENT_BALANCE"));
    }

    #[tokio::test]
    async fn a_result_without_an_invoice_fails_creation() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "pending" })),
            )
            .await;

        assert!(h
            .processor
            .create_payment_required(create_params())
            .await
            .is_err());
    }

    /// `create_payment_required` runs in the middleware's cancellation-blind
    /// initiation phase, so the client's response timeout is the only bound.
    #[tokio::test]
    async fn a_hung_wallet_fails_creation_inside_the_response_timeout() {
        let h = harness().await;
        h.wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        let started = tokio::time::Instant::now();
        let res = h.processor.create_payment_required(create_params()).await;
        assert!(res.is_err(), "a silent wallet must not hang creation");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must fail at the response timeout, took {:?}",
            started.elapsed()
        );
    }

    // ── verify_payment: polling ──────────────────────────────────────────────

    #[tokio::test]
    async fn settles_on_state_settled() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(
                    serde_json::json!({ "state": "settled", "payment_hash": "hash-9" }),
                ),
            )
            .await;

        let out = h
            .processor
            .verify_payment(verify_params("lnbc1", CancellationToken::new()))
            .await
            .expect("settles");
        assert_eq!(
            out.meta
                .unwrap()
                .get("payment_hash")
                .and_then(|v| v.as_str()),
            Some("hash-9")
        );
    }

    /// Wallets populate `state` or `settled_at`; requiring both would strand
    /// real payments.
    #[tokio::test]
    async fn settles_on_settled_at_without_a_state() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "settled_at": 1_700_000_000i64 })),
            )
            .await;

        let out = h
            .processor
            .verify_payment(verify_params("lnbc1", CancellationToken::new()))
            .await
            .expect("settles");
        // No payment_hash on the result, so the ts fallback receipt is used.
        assert_eq!(
            out.meta
                .unwrap()
                .get("payment_hash")
                .and_then(|v| v.as_str()),
            Some("settled:evt-1:1700000000")
        );
    }

    #[tokio::test]
    async fn fallback_receipt_without_settled_at() {
        let r = NwcInvoiceResult {
            state: Some("settled".into()),
            ..Default::default()
        };
        assert_eq!(receipt_for(&r, "evt-7"), "settled:evt-7");
    }

    #[tokio::test]
    async fn terminal_states_are_fatal() {
        for state in ["expired", "failed"] {
            let h = harness().await;
            h.wallet
                .set_behavior(
                    "lookup_invoice",
                    WalletBehavior::Answer(serde_json::json!({ "state": state })),
                )
                .await;

            let err = h
                .processor
                .verify_payment(verify_params("lnbc1", CancellationToken::new()))
                .await
                .expect_err("terminal state must fail");
            assert!(
                err.to_string().contains(state),
                "{state} should surface in the error"
            );
        }
    }

    /// A lagging wallet that has not seen the invoice is pending, not failed.
    #[tokio::test]
    async fn not_found_is_pending_and_other_codes_are_fatal() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Error {
                    code: "NOT_FOUND".to_string(),
                    message: "unknown invoice".to_string(),
                },
            )
            .await;

        let cancel = CancellationToken::new();
        let fut = h
            .processor
            .verify_payment(verify_params("lnbc1", cancel.clone()));
        // Still polling after the first NOT_FOUND rather than returning.
        let early = tokio::time::timeout(Duration::from_millis(200), fut).await;
        assert!(early.is_err(), "NOT_FOUND must keep the poller running");
        cancel.cancel();

        let h2 = harness().await;
        h2.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Error {
                    code: "INTERNAL".to_string(),
                    message: "boom".to_string(),
                },
            )
            .await;
        let err = h2
            .processor
            .verify_payment(verify_params("lnbc1", CancellationToken::new()))
            .await
            .expect_err("a non-NOT_FOUND code is fatal");
        assert!(err.to_string().contains("INTERNAL"));
    }

    /// A cancelled verify must stop the poller and must never read as settled.
    #[tokio::test]
    async fn a_cancelled_verify_errors_and_stops_polling() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "pending" })),
            )
            .await;

        let cancel = CancellationToken::new();
        let params = verify_params("lnbc1", cancel.clone());
        let handle = {
            let p = params.clone();
            let proc_ref = &h.processor;
            async move { proc_ref.verify_payment(p).await }
        };

        let (res, _) = tokio::join!(handle, async {
            tokio::time::sleep(Duration::from_millis(120)).await;
            cancel.cancel();
        });

        let err = res.expect_err("a cancelled verify must be an Err, never an empty Ok");
        assert!(err.to_string().contains("cancelled"));

        let before = h.wallet.call_count("lookup_invoice").await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            h.wallet.call_count("lookup_invoice").await,
            before,
            "the poller must stop hitting the wallet after cancellation"
        );
    }

    #[tokio::test]
    async fn prefers_lookup_by_payment_hash_once_known() {
        let h = harness().await;
        h.wallet
            .set_behavior("make_invoice", invoice_answer(Some("hash-from-create")))
            .await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "settled" })),
            )
            .await;

        let issued = h
            .processor
            .create_payment_required(create_params())
            .await
            .unwrap();
        h.processor
            .verify_payment(verify_params(&issued.pay_req, CancellationToken::new()))
            .await
            .expect("settles");

        let lookup = h
            .wallet
            .calls()
            .await
            .into_iter()
            .find(|(m, _)| m == "lookup_invoice")
            .expect("a lookup happened");
        assert_eq!(
            lookup.1["payment_hash"].as_str(),
            Some("hash-from-create"),
            "the cached hash from make_invoice should be preferred over the invoice string"
        );
        assert!(lookup.1.get("invoice").is_none());
    }

    #[tokio::test]
    async fn falls_back_to_invoice_lookup_when_no_hash_is_known() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "settled" })),
            )
            .await;

        h.processor
            .verify_payment(verify_params("lnbc-unknown", CancellationToken::new()))
            .await
            .expect("settles");

        let lookup = h.wallet.calls().await.into_iter().next().unwrap();
        assert_eq!(lookup.1["invoice"].as_str(), Some("lnbc-unknown"));
    }

    /// Duplicate delivery of one request must not double the wallet load.
    #[tokio::test]
    async fn dedupes_concurrent_verifies_for_the_same_invoice() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Delayed {
                    delay: Duration::from_millis(150),
                    result: serde_json::json!({ "state": "settled", "payment_hash": "h" }),
                },
            )
            .await;

        let cancel = CancellationToken::new();
        let (a, b, c) = tokio::join!(
            h.processor
                .verify_payment(verify_params("lnbc-same", cancel.clone())),
            h.processor
                .verify_payment(verify_params("lnbc-same", cancel.clone())),
            h.processor
                .verify_payment(verify_params("lnbc-same", cancel.clone())),
        );

        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        assert_eq!(
            h.wallet.call_count("lookup_invoice").await,
            1,
            "three concurrent verifies of one invoice must make one wallet call"
        );
    }

    #[tokio::test]
    async fn distinct_invoices_are_not_deduped_together() {
        let h = harness().await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "settled" })),
            )
            .await;

        let cancel = CancellationToken::new();
        let (a, b) = tokio::join!(
            h.processor
                .verify_payment(verify_params("inv-a", cancel.clone())),
            h.processor
                .verify_payment(verify_params("inv-b", cancel.clone())),
        );
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(h.wallet.call_count("lookup_invoice").await, 2);
    }

    // ── notification mode ────────────────────────────────────────────────────

    /// Auto mode probes the info event once and polls when the wallet does not
    /// advertise `payment_received`.
    #[tokio::test]
    async fn auto_mode_polls_when_notifications_are_unsupported() {
        let h = harness_with(LnBolt11NwcPaymentProcessorOptions {
            notification_verification: None,
            ..opts()
        })
        .await;
        h.wallet
            .publish_info_event(&["payment_sent"])
            .await
            .unwrap();
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "settled" })),
            )
            .await;

        h.processor
            .verify_payment(verify_params("lnbc1", CancellationToken::new()))
            .await
            .expect("settles by polling");
        assert_eq!(h.wallet.call_count("lookup_invoice").await, 1);
    }

    #[tokio::test]
    async fn notification_mode_resolves_from_payment_received() {
        let h = harness_with(LnBolt11NwcPaymentProcessorOptions {
            notification_verification: Some(true),
            ..opts()
        })
        .await;
        h.wallet
            .set_behavior("make_invoice", invoice_answer(Some("hash-notif")))
            .await;

        let issued = h
            .processor
            .create_payment_required(create_params())
            .await
            .unwrap();

        let client_pubkey = h.processor.inner.client.client_pubkey();
        let wallet = Arc::clone(&h.wallet);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let _ = wallet.settle(&client_pubkey, "hash-notif").await;
        });

        let out = h
            .processor
            .verify_payment(verify_params(&issued.pay_req, CancellationToken::new()))
            .await
            .expect("settles from the notification");
        assert_eq!(
            out.meta
                .unwrap()
                .get("payment_hash")
                .and_then(|v| v.as_str()),
            Some("hash-notif")
        );
        assert_eq!(
            h.wallet.call_count("lookup_invoice").await,
            0,
            "notification mode must not poll"
        );
    }

    /// Divergence from ts, which throws: with no cached payment_hash there is
    /// nothing to correlate a notification to, so fall back to polling rather
    /// than failing a payable invoice.
    #[tokio::test]
    async fn notification_mode_without_a_cached_hash_polls_instead_of_erroring() {
        let h = harness_with(LnBolt11NwcPaymentProcessorOptions {
            notification_verification: Some(true),
            ..opts()
        })
        .await;
        h.wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "settled" })),
            )
            .await;

        let out = h
            .processor
            .verify_payment(verify_params("lnbc-nohash", CancellationToken::new()))
            .await
            .expect("falls back to polling rather than erroring");
        assert!(out.meta.is_some());
        assert_eq!(h.wallet.call_count("lookup_invoice").await, 1);
    }

    #[tokio::test]
    async fn concurrent_verifies_subscribe_for_notifications_exactly_once() {
        let h = harness_with(LnBolt11NwcPaymentProcessorOptions {
            notification_verification: Some(true),
            ..opts()
        })
        .await;
        h.wallet
            .set_behavior("make_invoice", invoice_answer(Some("hash-multi")))
            .await;
        let issued = h
            .processor
            .create_payment_required(create_params())
            .await
            .unwrap();

        let client_pubkey = h.processor.inner.client.client_pubkey();
        let wallet = Arc::clone(&h.wallet);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = wallet.settle(&client_pubkey, "hash-multi").await;
        });

        let cancel = CancellationToken::new();
        // Distinct pay_reqs so the in-flight dedup does not mask the
        // subscription race; both map to the same hash via the cache.
        h.processor
            .inner
            .cache_payment_hash("lnbc-second", "hash-multi")
            .await;
        let (a, b) = tokio::join!(
            h.processor
                .verify_payment(verify_params(&issued.pay_req, cancel.clone())),
            h.processor
                .verify_payment(verify_params("lnbc-second", cancel.clone())),
        );

        assert!(a.is_ok(), "first verify: {a:?}");
        assert!(b.is_ok(), "second verify: {b:?}");
        assert!(
            h.processor.inner.notifications_subscribed.initialized(),
            "the subscription is installed exactly once"
        );
    }

    #[tokio::test]
    async fn a_cancelled_notification_wait_errors_and_drops_its_waiter() {
        let h = harness_with(LnBolt11NwcPaymentProcessorOptions {
            notification_verification: Some(true),
            ..opts()
        })
        .await;
        h.processor
            .inner
            .cache_payment_hash("lnbc-cancel", "hash-cancel")
            .await;

        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            c2.cancel();
        });

        let err = h
            .processor
            .verify_payment(verify_params("lnbc-cancel", cancel))
            .await
            .expect_err("a cancelled notification wait must be an Err");
        assert!(err.to_string().contains("cancelled"));

        assert!(
            h.processor
                .inner
                .waiters
                .lock()
                .await
                .get("hash-cancel")
                .is_none_or(|v| v.is_empty()),
            "the cancelled waiter must not be retained"
        );
    }

    // ── backoff + hygiene ────────────────────────────────────────────────────

    #[tokio::test]
    async fn backoff_follows_the_ts_schedule_floored_at_the_poll_interval() {
        let h = harness_with(LnBolt11NwcPaymentProcessorOptions {
            poll_interval: Duration::from_millis(1500),
            ..opts()
        })
        .await;
        let inner = &h.processor.inner;

        // Floored: every early step is below the 1500 ms floor.
        for attempt in 0..4 {
            let d = inner.next_delay(attempt).as_millis() as u64;
            assert!(
                (1500..1500 + JITTER_MAX_MS).contains(&d),
                "attempt {attempt} gave {d}ms, expected the floor plus jitter"
            );
        }
        // Past the floor the schedule takes over and grows.
        let late = inner.next_delay(8).as_millis() as u64;
        assert!(
            (15_000..15_000 + JITTER_MAX_MS).contains(&late),
            "attempt 8 gave {late}ms, expected 15000 plus jitter"
        );
        // Saturates at the last entry rather than panicking on overrun.
        assert_eq!(
            inner.next_delay(99).as_millis() as u64 / 1000,
            inner.next_delay(8).as_millis() as u64 / 1000
        );
    }

    #[tokio::test]
    async fn an_unfloored_schedule_grows_monotonically() {
        let h = harness_with(LnBolt11NwcPaymentProcessorOptions {
            poll_interval: Duration::from_millis(1),
            ..opts()
        })
        .await;
        let inner = &h.processor.inner;
        for attempt in 0..BACKOFF_SCHEDULE_MS.len() - 1 {
            let a = BACKOFF_SCHEDULE_MS[attempt];
            let b = BACKOFF_SCHEDULE_MS[attempt + 1];
            assert!(a < b, "schedule must increase: {a} then {b}");
            let d = inner.next_delay(attempt).as_millis() as u64;
            assert!(
                (a..a + JITTER_MAX_MS).contains(&d),
                "attempt {attempt}: {d}ms"
            );
        }
    }

    /// Whether `call` binds `name` as a tracing field, i.e. `name = ...`.
    ///
    /// Boundary-aware on purpose. A plain substring check reports
    /// `has_preimage = ...` (a boolean, which is exactly what we DO want
    /// logged) as if it were `preimage = ...`, and reports the word "invoice"
    /// inside a message like "NWC lookup_invoice result".
    fn binds_field(call: &str, name: &str) -> bool {
        call.match_indices(name).any(|(i, _)| {
            let preceded_by_ident = call[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if preceded_by_ident {
                return false;
            }
            let after = call[i + name.len()..].trim_start();
            after.starts_with('=') && !after.starts_with("==")
        })
    }

    /// An invoice is a bearer payment request: anyone holding it can be paid
    /// against it, and logs leak to aggregators. No tracing call may carry it.
    ///
    /// Scans whole macro invocations rather than single lines, because a
    /// `tracing::debug!` spans several lines and its fields are never on the
    /// first one.
    #[test]
    fn no_tracing_call_carries_the_invoice() {
        let src = include_str!("ln_bolt11_nwc.rs");
        // Only the implementation, not this test module, which necessarily
        // mentions the very identifiers it is checking for.
        let impl_src = &src[..src
            .find("#[cfg(all(test, feature = \"test-utils\"))]")
            .unwrap_or(src.len())];

        let mut checked = 0usize;
        for (idx, _) in impl_src.match_indices("tracing::") {
            let rest = &impl_src[idx..];
            let end = rest.find(");").map(|e| e + 2).unwrap_or(rest.len());
            let call = &rest[..end];
            checked += 1;
            for banned in ["pay_req", "invoice", "preimage", "secret"] {
                assert!(
                    !binds_field(call, banned),
                    "a tracing call binds `{banned}` as a field:
{call}"
                );
            }
        }
        assert!(
            checked >= 4,
            "expected to scan several tracing calls, saw {checked}"
        );
    }
}
