//! Minimal NIP-47 client: request/response and notifications over a
//! [`RelayPoolTrait`], with no dependency on the `nwc` crate.
//!
//! # Why not the `nwc` crate
//!
//! `nwc` owns its relay pool privately, so nothing built on it can be driven by
//! [`MockRelayPool`](crate::relay::MockRelayPool) and every test would need a
//! live wallet. This client takes its pool by construction, so the same code
//! path runs against a scripted mock wallet in CI and a real wallet in
//! production.
//!
//! # The client owns a relay pool
//!
//! Prefer [`NwcClient::connect`], which builds a pool from the connection's own
//! relays. [`NwcClient::with_pool`] exists for tests and for a caller that
//! genuinely wants to share one, and it carries a sharp edge: on a real
//! [`RelayPool`](crate::relay::RelayPool), `connect` *adds* the wallet's relays
//! to whatever pool it is given, and the NIP-47 subscription is permanent
//! because [`RelayPoolTrait`] has no unsubscribe. Handing it a transport's pool
//! therefore publishes ContextVM traffic to the wallet's relays and leaves a
//! wallet filter on the transport's relays for the life of the process.
//!
//! # One subscription, not one per request
//!
//! ts-sdk opens a subscription per request and closes it on settle.
//! [`RelayPoolTrait`] has no unsubscribe, so transliterating that would leak
//! one live REQ per payment. This client opens a single subscription covering
//! responses and notifications, runs one reader task, and correlates in process
//! through a waiter map keyed by the request event id.
//!
//! # Divergences from ts-sdk
//!
//! * Notifications are decrypted **by ciphertext shape**, not by event kind: a
//!   NIP-04 payload carries `?iv=`. ts assumes NIP-04 for both kinds and so
//!   drops NIP-44 notifications; keying on kind instead would double-deliver
//!   for a wallet that publishes 23196 and 23197 for one event.
//! * Requests are issued concurrently and correlated by id, where ts serializes
//!   them behind a promise queue.
//! * Every optional response field tolerates a wrong JSON type (see
//!   [`types`](super::types)).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;

use crate::payments::nip47::error::NwcError;
use crate::payments::nip47::types::{
    sanitize_wallet_text, NwcInvoiceResult, NwcNotificationPayload, NwcPayInvoiceResult,
    NwcResponseEnvelope,
};
use crate::payments::nip47::uri::NwcConnection;
use crate::relay::{RelayPool, RelayPoolTrait};

const LOG_TARGET: &str = "contextvm::payments::nip47";

/// Kind 13194: the wallet service's info event.
const KIND_INFO: u16 = 13194;
/// Kind 23194: a client request to the wallet.
const KIND_REQUEST: u16 = 23194;
/// Kind 23195: the wallet's response.
const KIND_RESPONSE: u16 = 23195;
/// Kind 23196: a notification, historically NIP-04.
const KIND_NOTIFICATION_LEGACY: u16 = 23196;
/// Kind 23197: a notification, historically NIP-44.
const KIND_NOTIFICATION: u16 = 23197;

/// Default deadline for one NIP-47 call.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// How far back the subscription looks on start.
///
/// ts uses 5 s. Relay clocks drift and a reconnect can land well after the
/// original subscribe, so a slightly wider window is safer than missing a live
/// answer; it is still bounded, so a relay that retains ephemeral events does
/// not replay this client's whole history into the sinks.
const SUBSCRIPTION_LOOKBACK: Duration = Duration::from_secs(120);

/// Construction options for [`NwcClient`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NwcClientOptions {
    /// Deadline covering start-up, publish and the wait for an answer.
    /// Default 60 s. Must be non-zero.
    pub response_timeout: Duration,
}

impl Default for NwcClientOptions {
    fn default() -> Self {
        Self {
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
        }
    }
}

impl NwcClientOptions {
    /// Options with the default response timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the deadline covering start-up, publish and the wait for an answer.
    ///
    /// A builder rather than a public field literal: the struct is
    /// `#[non_exhaustive]`, so a downstream crate cannot name it in a struct
    /// expression.
    pub fn with_response_timeout(mut self, response_timeout: Duration) -> Self {
        self.response_timeout = response_timeout;
        self
    }

    /// Reject a configuration that cannot work.
    fn validate(&self) -> Result<(), NwcError> {
        if self.response_timeout.is_zero() {
            return Err(NwcError::Config {
                reason: "response_timeout must be non-zero; a zero deadline would publish the \
                         request and then always time out"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// A registered notification sink.
type NotificationSink = Arc<dyn Fn(NwcNotificationPayload) + Send + Sync>;

struct Inner {
    /// Pending request-event-id to response-plaintext waiters.
    ///
    /// A `std::sync::Mutex`: no critical section crosses an await, and the RAII
    /// guard that de-registers a dropped waiter cannot await.
    waiters: Mutex<HashMap<EventId, oneshot::Sender<String>>>,
    notification_sinks: AsyncMutex<Vec<NotificationSink>>,
    /// Set once any response has correlated. Until then the filter may be too
    /// narrow for the wallet in front of us.
    ever_correlated: AtomicBool,
    /// Set by `close()`, by `Drop`, and when the reader exits.
    closed: AtomicBool,
}

impl Inner {
    /// Drop every waiter so its `request()` returns instead of waiting out the
    /// full deadline. Dropping the sender closes the channel.
    fn drain_waiters(&self) {
        if let Ok(mut map) = self.waiters.lock() {
            map.clear();
        }
    }
}

/// Removes its waiter on drop, including when the `request()` future is
/// cancelled mid-wait.
///
/// Without this, a dropped future (every cancelled verification in the payment
/// processor's `select!`) orphans an `EventId` and a `oneshot::Sender` in the
/// map for the life of the process.
struct WaiterGuard {
    inner: Arc<Inner>,
    id: EventId,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.inner.waiters.lock() {
            map.remove(&self.id);
        }
    }
}

/// A NIP-47 client bound to one wallet connection and one relay pool.
pub struct NwcClient {
    pool: Arc<dyn RelayPoolTrait>,
    connection: NwcConnection,
    keys: Keys,
    response_timeout: Duration,
    inner: Arc<Inner>,
    started: AsyncMutex<bool>,
    cancel: CancellationToken,
}

impl NwcClient {
    /// Build a client that owns a relay pool connected to the wallet's own
    /// relays. This is the constructor production code should use.
    ///
    /// # Errors
    ///
    /// [`NwcError::Config`] for invalid options, [`NwcError::Connect`] if the
    /// pool cannot be created.
    pub async fn connect(
        connection: NwcConnection,
        options: NwcClientOptions,
    ) -> Result<Self, NwcError> {
        options.validate()?;
        let pool = RelayPool::new(Keys::generate())
            .await
            .map_err(|e| NwcError::Connect {
                reason: e.to_string(),
            })?;
        Ok(Self::build(Arc::new(pool), connection, options))
    }

    /// Build a client over a caller-supplied pool.
    ///
    /// Intended for tests, and for a caller that has deliberately decided to
    /// share. See the module docs for why sharing a transport's pool is a trap.
    ///
    /// # Errors
    ///
    /// [`NwcError::Config`] when the options are invalid.
    pub fn with_pool(
        pool: Arc<dyn RelayPoolTrait>,
        connection: NwcConnection,
        options: NwcClientOptions,
    ) -> Result<Self, NwcError> {
        options.validate()?;
        Ok(Self::build(pool, connection, options))
    }

    fn build(
        pool: Arc<dyn RelayPoolTrait>,
        connection: NwcConnection,
        options: NwcClientOptions,
    ) -> Self {
        let keys = Keys::new(connection.secret.clone());
        Self {
            pool,
            connection,
            keys,
            response_timeout: options.response_timeout,
            inner: Arc::new(Inner {
                waiters: Mutex::new(HashMap::new()),
                notification_sinks: AsyncMutex::new(Vec::new()),
                ever_correlated: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
            started: AsyncMutex::new(false),
            cancel: CancellationToken::new(),
        }
    }

    /// The client public key this connection presents to the wallet.
    pub fn client_pubkey(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// The wallet service public key.
    pub fn wallet_pubkey(&self) -> PublicKey {
        self.connection.wallet_pubkey
    }

    /// Stop the reader and refuse further requests. Idempotent.
    ///
    /// Fail-fast: a request after this returns [`NwcError::Closed`] without
    /// publishing, so a shut-down client cannot still cause a payment. Waiting
    /// callers are released rather than left to time out.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        self.inner.drain_waiters();
    }

    /// Whether [`Self::close`] has been called, or the reader has exited.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Filters this client subscribes with.
    ///
    /// `narrow` additionally requires the `p` tag addressed to us, which every
    /// conformant wallet sets. The widened form drops it: see
    /// [`Self::widen_filter_if_never_correlated`].
    fn filters(&self, narrow: bool) -> Vec<Filter> {
        let since = Timestamp::now() - SUBSCRIPTION_LOOKBACK;
        let base = Filter::new()
            .kinds([
                Kind::from(KIND_RESPONSE),
                Kind::from(KIND_NOTIFICATION_LEGACY),
                Kind::from(KIND_NOTIFICATION),
            ])
            .author(self.connection.wallet_pubkey)
            .since(since);
        vec![if narrow {
            base.pubkey(self.keys.public_key())
        } else {
            base
        }]
    }

    /// Connect, subscribe, and spawn the reader task. Idempotent.
    async fn ensure_started(&self) -> Result<(), NwcError> {
        if self.is_closed() {
            return Err(NwcError::Closed);
        }
        let mut started = self.started.lock().await;
        if *started {
            return Ok(());
        }

        self.pool
            .connect(&self.connection.relay_urls())
            .await
            .map_err(|e| NwcError::Connect {
                reason: e.to_string(),
            })?;

        // Take the receiver BEFORE subscribing so a replay cannot land before
        // we are listening.
        let notifications = self.pool.notifications();

        self.pool
            .subscribe(self.filters(true))
            .await
            .map_err(|e| NwcError::Subscribe {
                reason: e.to_string(),
            })?;

        let inner = Arc::clone(&self.inner);
        let keys = self.keys.clone();
        let wallet = self.connection.wallet_pubkey;
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            Self::reader_loop(notifications, Arc::clone(&inner), keys, wallet, cancel).await;
            // The rail is dead once the reader stops. Mark it so a later
            // request fails fast instead of publishing into a void and waiting
            // out the deadline, and release anyone already waiting.
            inner.closed.store(true, Ordering::SeqCst);
            inner.drain_waiters();
            tracing::warn!(target: LOG_TARGET, "NWC reader stopped; client is now closed");
        });

        *started = true;
        Ok(())
    }

    /// Re-subscribe without the `p` tag.
    ///
    /// Our filter requires `#p`, which every conformant wallet sets but which
    /// is stricter than ts (whose per-request REQ has no `#p` at all). A wallet
    /// that omits it would be invisible forever. While nothing has *ever*
    /// correlated, widen once so such a wallet still works; after the first
    /// successful correlation the narrow filter is known good and this is never
    /// attempted again.
    async fn widen_filter_if_never_correlated(&self) {
        if self.inner.ever_correlated.load(Ordering::SeqCst) {
            return;
        }
        tracing::debug!(
            target: LOG_TARGET,
            "no NWC response has correlated yet; re-subscribing without the p-tag filter"
        );
        if let Err(e) = self.pool.subscribe(self.filters(false)).await {
            tracing::debug!(target: LOG_TARGET, error = %e, "widened NWC subscribe failed");
        }
    }

    /// Single reader task: decrypts, correlates responses, fans out notifications.
    ///
    /// Each event is handled inside `catch_unwind`, so one panicking
    /// notification sink (caller code) cannot take the rail down for the life
    /// of the process.
    async fn reader_loop(
        mut notifications: tokio::sync::broadcast::Receiver<RelayPoolNotification>,
        inner: Arc<Inner>,
        keys: Keys,
        wallet_pubkey: PublicKey,
        cancel: CancellationToken,
    ) {
        loop {
            let notification = tokio::select! {
                _ = cancel.cancelled() => break,
                result = notifications.recv() => match result {
                    Ok(n) => n,
                    // Lagging drops events but must not kill the reader: the
                    // waiter map survives, so an in-flight request still
                    // resolves from a later delivery or fails at its deadline.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            skipped,
                            "NWC relay broadcast lagged, continuing"
                        );
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            };

            let RelayPoolNotification::Event { event, .. } = notification else {
                continue;
            };

            // A stranger cannot answer for the wallet. `nostr-relay-pool`
            // verifies signatures but not filter matches
            // (`verify_subscriptions` defaults to false), so a relay that
            // ignores `authors` would otherwise let a correctly correlated,
            // stranger-signed "settled" through.
            if event.pubkey != wallet_pubkey {
                continue;
            }

            let kind = event.kind.as_u16();
            let sinks = if matches!(kind, KIND_NOTIFICATION | KIND_NOTIFICATION_LEGACY) {
                inner.notification_sinks.lock().await.clone()
            } else {
                Vec::new()
            };

            // Isolate per event: a panic here is contained to this event.
            let inner_for_event = Arc::clone(&inner);
            let keys_for_event = keys.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Self::handle_event(&event, &inner_for_event, &keys_for_event, kind, &sinks)
            }));
            if outcome.is_err() {
                tracing::error!(
                    target: LOG_TARGET,
                    kind,
                    "a NWC event handler panicked; the reader is continuing"
                );
            }
        }
        tracing::debug!(target: LOG_TARGET, "NWC reader loop stopped");
    }

    /// Synchronous per-event handling, so it can be wrapped in `catch_unwind`.
    fn handle_event(
        event: &Event,
        inner: &Arc<Inner>,
        keys: &Keys,
        kind: u16,
        sinks: &[NotificationSink],
    ) {
        match kind {
            KIND_RESPONSE => Self::handle_response(event, inner, keys),
            KIND_NOTIFICATION | KIND_NOTIFICATION_LEGACY => {
                Self::handle_notification(event, keys, sinks)
            }
            _ => {}
        }
    }

    fn handle_response(event: &Event, inner: &Arc<Inner>, keys: &Keys) {
        let Some(request_id) = first_e_tag(event) else {
            return;
        };

        let waiter = inner
            .waiters
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&request_id));
        let Some(waiter) = waiter else {
            tracing::debug!(
                target: LOG_TARGET,
                request_id = %request_id.to_hex(),
                "NWC response did not correlate to a pending request"
            );
            return;
        };

        match decrypt_auto(keys, &event.pubkey, &event.content) {
            Ok(plaintext) => {
                inner.ever_correlated.store(true, Ordering::SeqCst);
                let _ = waiter.send(plaintext);
            }
            Err(reason) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    reason = %reason,
                    "failed to decrypt a NWC response; the caller will fail at its deadline"
                );
            }
        }
    }

    fn handle_notification(event: &Event, keys: &Keys, sinks: &[NotificationSink]) {
        // By ciphertext shape, not by kind: a wallet supporting both
        // encryptions publishes 23196 and 23197 for one notification, and ts's
        // own fixture is NIP-04 content on kind 23197.
        let Ok(plaintext) = decrypt_auto(keys, &event.pubkey, &event.content) else {
            tracing::debug!(target: LOG_TARGET, "failed to decrypt a NWC notification");
            return;
        };

        let Ok(payload) = serde_json::from_str::<NwcNotificationPayload>(&plaintext) else {
            tracing::debug!(target: LOG_TARGET, "malformed NWC notification payload");
            return;
        };

        for sink in sinks {
            sink(payload.clone());
        }
    }

    /// Register a sink invoked for every decoded notification.
    ///
    /// Sinks run on the reader task and must not block. A panicking sink is
    /// contained to its own event rather than killing the rail, though it will
    /// skip the remaining sinks for that event.
    ///
    /// Side effect worth knowing: this starts the subscription and the reader
    /// if they are not running yet.
    ///
    /// # Errors
    ///
    /// [`NwcError::Closed`], [`NwcError::Connect`] or [`NwcError::Subscribe`].
    pub async fn on_notification<F>(&self, sink: F) -> Result<(), NwcError>
    where
        F: Fn(NwcNotificationPayload) + Send + Sync + 'static,
    {
        self.ensure_started().await?;
        self.inner
            .notification_sinks
            .lock()
            .await
            .push(Arc::new(sink));
        Ok(())
    }

    /// Issue a NIP-47 request and await its correlated response.
    ///
    /// `response_timeout` is one deadline over **start-up, publish and the
    /// wait**, not the wait alone: a hanging connect or publish fails at the
    /// same bound rather than parking the caller indefinitely.
    ///
    /// A wallet-reported error is returned as `Ok(envelope)` with `error` set,
    /// not `Err`: the caller decides whether a given code is fatal or merely
    /// pending. Use [`NwcResponseEnvelope::into_result`] when that distinction
    /// does not matter.
    ///
    /// Crate-internal: the typed [`Self::make_invoice`], [`Self::lookup_invoice`]
    /// and [`Self::pay_invoice`] are the public surface, so adding a NIP field
    /// later is not a breaking change.
    ///
    /// # Errors
    ///
    /// [`NwcError::Closed`] if the client is shut down, [`NwcError::Timeout`]
    /// at the deadline (carrying whether the request was published),
    /// [`NwcError::Publish`], [`NwcError::Crypto`] or
    /// [`NwcError::MalformedResponse`].
    pub(crate) async fn request<P, R>(
        &self,
        method: &str,
        params: P,
        expiration: Option<Duration>,
    ) -> Result<NwcResponseEnvelope<R>, NwcError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let deadline = tokio::time::Instant::now() + self.response_timeout;
        let timeout_ms = self.response_timeout.as_millis() as u64;

        if self.is_closed() {
            return Err(NwcError::Closed);
        }

        match tokio::time::timeout_at(deadline, self.ensure_started()).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(NwcError::Timeout {
                    method: method.to_string(),
                    timeout_ms,
                    published: false,
                })
            }
        }

        let body = serde_json::json!({ "method": method, "params": params });
        let plaintext = serde_json::to_string(&body).map_err(|e| NwcError::Crypto {
            direction: "encryption",
            reason: format!("request could not be serialized ({:?})", e.classify()),
        })?;

        let content = nip04::encrypt(
            self.keys.secret_key(),
            &self.connection.wallet_pubkey,
            plaintext,
        )
        .map_err(|e| NwcError::Crypto {
            direction: "encryption",
            reason: sanitize_wallet_text(&e.to_string()),
        })?;

        let mut tags = vec![
            Tag::public_key(self.connection.wallet_pubkey),
            Tag::parse(["encryption", "nip04"]).map_err(|e| NwcError::Crypto {
                direction: "encryption",
                reason: e.to_string(),
            })?,
        ];
        if let Some(expiry) = expiration {
            // NIP-40 expiration, as ts sends: a wallet that honors it will not
            // act on a request the caller has already given up on.
            let at = Timestamp::now() + expiry;
            tags.push(
                Tag::parse(["expiration", &at.as_secs().to_string()]).map_err(|e| {
                    NwcError::Crypto {
                        direction: "encryption",
                        reason: e.to_string(),
                    }
                })?,
            );
        }

        let event = EventBuilder::new(Kind::from(KIND_REQUEST), content)
            .tags(tags)
            .sign_with_keys(&self.keys)
            .map_err(|e| NwcError::Crypto {
                direction: "encryption",
                reason: e.to_string(),
            })?;
        let request_id = event.id;

        // Register the waiter BEFORE publishing, so an instant answer cannot
        // arrive before anything is listening. The guard removes it on every
        // exit path, including this future being dropped mid-wait.
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.inner.waiters.lock().map_err(|_| NwcError::Closed)?;
            map.insert(request_id, tx);
        }
        let _guard = WaiterGuard {
            inner: Arc::clone(&self.inner),
            id: request_id,
        };

        let published =
            match tokio::time::timeout_at(deadline, self.pool.publish_event(&event)).await {
                Ok(Ok(_)) => true,
                Ok(Err(e)) => {
                    return Err(NwcError::Publish {
                        method: method.to_string(),
                        reason: e.to_string(),
                    })
                }
                Err(_) => {
                    return Err(NwcError::Timeout {
                        method: method.to_string(),
                        timeout_ms,
                        published: false,
                    })
                }
            };

        tracing::debug!(
            target: LOG_TARGET,
            method,
            request_id = %request_id.to_hex(),
            "NWC request published"
        );

        // One chance to widen a too-narrow filter, while nothing has ever
        // correlated. Only on the very first requests.
        self.widen_filter_if_never_correlated().await;

        let plaintext = match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(p)) => p,
            // Sender dropped: the client closed, or the reader died.
            Ok(Err(_)) => return Err(NwcError::Closed),
            Err(_) => {
                return Err(NwcError::Timeout {
                    method: method.to_string(),
                    timeout_ms,
                    published,
                })
            }
        };

        let envelope: NwcResponseEnvelope<R> =
            serde_json::from_str(&plaintext).map_err(|e| NwcError::from_serde(method, &e))?;

        if let Some(kind) = envelope.result_type.as_deref() {
            if kind != method {
                // Correlation by event id already proves which request this
                // answers, so a mismatched label is not grounds to discard a
                // settlement. Surface it and carry on.
                tracing::warn!(
                    target: LOG_TARGET,
                    method,
                    result_type = kind,
                    "NWC result_type does not match the request method"
                );
            }
        }

        Ok(envelope)
    }

    /// Ask the wallet for a BOLT11 invoice.
    ///
    /// # Errors
    ///
    /// [`NwcError::InvalidAmount`] if `amount_msats` is zero; otherwise see
    /// [`Self::request`].
    pub async fn make_invoice(
        &self,
        amount_msats: u64,
        description: Option<&str>,
        expiry: Option<Duration>,
    ) -> Result<NwcResponseEnvelope<NwcInvoiceResult>, NwcError> {
        if amount_msats == 0 {
            return Err(NwcError::InvalidAmount {
                reason: "make_invoice amount must be positive".to_string(),
            });
        }
        let mut params = serde_json::json!({ "amount": amount_msats });
        if let Some(d) = description {
            params["description"] = serde_json::Value::String(d.to_string());
        }
        if let Some(e) = expiry {
            params["expiry"] = serde_json::Value::from(e.as_secs());
        }
        self.request("make_invoice", params, None).await
    }

    /// Look an invoice up, by payment hash when known and by invoice otherwise.
    ///
    /// # Errors
    ///
    /// [`NwcError::Config`] if neither identifier is given; otherwise see
    /// [`Self::request`].
    pub async fn lookup_invoice(
        &self,
        payment_hash: Option<&str>,
        invoice: Option<&str>,
    ) -> Result<NwcResponseEnvelope<NwcInvoiceResult>, NwcError> {
        let params = match (payment_hash, invoice) {
            (Some(h), _) => serde_json::json!({ "payment_hash": h }),
            (None, Some(i)) => serde_json::json!({ "invoice": i }),
            (None, None) => {
                return Err(NwcError::Config {
                    reason: "lookup_invoice needs a payment_hash or an invoice".to_string(),
                })
            }
        };
        self.request("lookup_invoice", params, None).await
    }

    /// Pay a BOLT11 invoice.
    ///
    /// `expiration` sets a NIP-40 expiration on the request, so a wallet that
    /// honors it will not pay after the caller has given up. Strongly
    /// recommended: `pay_invoice` is not idempotent.
    ///
    /// # Errors
    ///
    /// See [`Self::request`]. On [`NwcError::Timeout`], check
    /// [`NwcError::may_have_reached_wallet`] before retrying.
    pub async fn pay_invoice(
        &self,
        invoice: &str,
        expiration: Option<Duration>,
    ) -> Result<NwcResponseEnvelope<NwcPayInvoiceResult>, NwcError> {
        self.request(
            "pay_invoice",
            serde_json::json!({ "invoice": invoice }),
            expiration,
        )
        .await
    }

    /// Fetch the notification types the wallet advertises on its kind-13194
    /// info event.
    ///
    /// Returns an empty list when no info event is found, which callers read as
    /// "no notification support" and fall back to polling.
    ///
    /// Side effect worth knowing: this starts the subscription and the reader
    /// if they are not running yet.
    ///
    /// # Errors
    ///
    /// [`NwcError::Closed`], [`NwcError::Connect`] or [`NwcError::Subscribe`].
    pub async fn fetch_info_notification_types(&self) -> Result<Vec<String>, NwcError> {
        self.ensure_started().await?;

        let filter = Filter::new()
            .kind(Kind::from(KIND_INFO))
            .author(self.connection.wallet_pubkey)
            .limit(1);

        let events = self
            .pool
            .fetch_events(vec![filter], self.response_timeout)
            .await
            .map_err(|e| NwcError::Subscribe {
                reason: e.to_string(),
            })?;

        let Some(event) = events.into_iter().next() else {
            return Ok(Vec::new());
        };

        // ts joins every `notifications` tag value with a space then splits on
        // whitespace, so a wallet may use one tag with a space-separated list
        // or several tags. Reproduce both.
        let mut types = Vec::new();
        for tag in event.tags.iter() {
            let slice = tag.as_slice();
            if slice.first().map(String::as_str) != Some("notifications") {
                continue;
            }
            for value in slice.iter().skip(1) {
                for t in value.split_whitespace() {
                    let t = t.trim();
                    if !t.is_empty() && !types.iter().any(|e: &String| e == t) {
                        types.push(t.to_string());
                    }
                }
            }
        }
        Ok(types)
    }

    /// Number of waiters currently registered. Test-only introspection, so the
    /// RAII guard's effect is observable.
    #[cfg(test)]
    pub(crate) fn waiter_count(&self) -> usize {
        self.inner.waiters.lock().map(|m| m.len()).unwrap_or(0)
    }
}

impl Drop for NwcClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// First `e` tag of an event, as an [`EventId`].
///
/// Parity note: ts takes the first `e` tag too. Neither implementation handles
/// an answer carrying several `e` tags sensibly, which no conformant wallet
/// sends.
fn first_e_tag(event: &Event) -> Option<EventId> {
    event.tags.iter().find_map(|t| match t.as_slice() {
        [name, value, ..] if name == "e" => EventId::from_hex(value).ok(),
        _ => None,
    })
}

/// Decrypt by ciphertext shape rather than by event kind.
///
/// NIP-04 ciphertext is `<base64>?iv=<base64>`; NIP-44 has no `?iv=`. Keying on
/// shape means a NIP-04 payload on kind 23197 (ts's own fixture) decodes, and a
/// wallet that publishes both kinds for one notification is not mis-handled by
/// kind alone.
fn decrypt_auto(keys: &Keys, author: &PublicKey, content: &str) -> Result<String, String> {
    if content.contains("?iv=") {
        nip04::decrypt(keys.secret_key(), author, content).map_err(|e| e.to_string())
    } else {
        nip44::decrypt(keys.secret_key(), author, content).map_err(|e| e.to_string())
    }
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::payments::nip47::mock_wallet::{MockWallet, MockWalletQuirks, WalletBehavior};
    use crate::payments::nip47::types::{NwcErrorBody, NOTIFICATION_PAYMENT_RECEIVED};
    use crate::payments::nip47::uri::parse_nwc_uri;
    use crate::relay::MockRelayPool;

    /// Wallet + client on one mock relay network, wired through a real NWC URI.
    async fn harness() -> (Arc<MockWallet>, NwcClient, Arc<MockRelayPool>) {
        harness_with_timeout(Duration::from_secs(5)).await
    }

    async fn harness_with_timeout(
        response_timeout: Duration,
    ) -> (Arc<MockWallet>, NwcClient, Arc<MockRelayPool>) {
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

        let client = NwcClient::with_pool(
            Arc::clone(&client_pool) as Arc<dyn RelayPoolTrait>,
            connection,
            NwcClientOptions::new().with_response_timeout(response_timeout),
        )
        .expect("options valid");
        (wallet, client, client_pool)
    }

    fn invoice_result(invoice: &str) -> serde_json::Value {
        serde_json::json!({ "invoice": invoice, "state": "pending" })
    }

    // ── round trips ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn round_trips_the_three_typed_methods() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(
                    serde_json::json!({ "invoice": "lnbc210n1p", "payment_hash": "hash-1" }),
                ),
            )
            .await;
        wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "settled" })),
            )
            .await;
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::json!({ "preimage": "pre", "fees_paid": 12 })),
            )
            .await;

        let made = client
            .make_invoice(21_000, Some("a tool call"), Some(Duration::from_secs(300)))
            .await
            .expect("make_invoice")
            .into_result()
            .expect("no wallet error");
        assert_eq!(made.invoice.as_deref(), Some("lnbc210n1p"));

        let looked = client
            .lookup_invoice(Some("hash-1"), None)
            .await
            .expect("lookup_invoice")
            .into_result()
            .expect("no wallet error");
        assert!(looked.is_settled());

        let paid = client
            .pay_invoice("lnbc1", Some(Duration::from_secs(60)))
            .await
            .expect("pay_invoice")
            .into_result()
            .expect("no wallet error");
        assert_eq!(paid.preimage.as_deref(), Some("pre"));

        // The typed wrappers send what the NIP expects.
        let calls = wallet.calls().await;
        let make = calls.iter().find(|(m, _)| m == "make_invoice").unwrap();
        assert_eq!(make.1["amount"].as_u64(), Some(21_000));
        assert_eq!(make.1["expiry"].as_u64(), Some(300));
        let look = calls.iter().find(|(m, _)| m == "lookup_invoice").unwrap();
        assert_eq!(look.1["payment_hash"].as_str(), Some("hash-1"));
    }

    #[tokio::test]
    async fn lookup_falls_back_to_the_invoice_when_no_hash_is_known() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "settled" })),
            )
            .await;
        client
            .lookup_invoice(None, Some("lnbc-xyz"))
            .await
            .expect("lookup_invoice");
        let call = wallet.calls().await.into_iter().next().unwrap();
        assert_eq!(call.1["invoice"].as_str(), Some("lnbc-xyz"));
    }

    #[tokio::test]
    async fn lookup_without_either_identifier_is_a_config_error() {
        let (_w, client, _p) = harness().await;
        assert!(matches!(
            client.lookup_invoice(None, None).await,
            Err(NwcError::Config { .. })
        ));
    }

    #[tokio::test]
    async fn make_invoice_rejects_a_zero_amount_before_the_wallet() {
        let (wallet, client, _p) = harness().await;
        assert!(matches!(
            client.make_invoice(0, None, None).await,
            Err(NwcError::InvalidAmount { .. })
        ));
        assert_eq!(wallet.call_count("make_invoice").await, 0);
    }

    // ── errors and robustness ────────────────────────────────────────────────

    #[tokio::test]
    async fn wallet_error_is_surfaced_not_read_as_success() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Error {
                    code: "INSUFFICIENT_BALANCE".to_string(),
                    message: "no funds".to_string(),
                },
            )
            .await;

        let env = client.pay_invoice("lnbc1", None).await.expect("call ok");
        assert_eq!(
            env.error.as_ref().map(NwcErrorBody::code_upper).as_deref(),
            Some("INSUFFICIENT_BALANCE")
        );
        assert!(env.into_result().is_err());
    }

    /// An unscripted method gets `NOT_IMPLEMENTED` from a real wallet.
    #[tokio::test]
    async fn an_unimplemented_method_surfaces_the_wallets_code() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_quirks(MockWalletQuirks {
                not_implemented_for_unscripted: true,
                ..Default::default()
            })
            .await;
        let env = client.pay_invoice("lnbc1", None).await.expect("call ok");
        assert_eq!(
            env.error.as_ref().map(NwcErrorBody::code_upper).as_deref(),
            Some("NOT_IMPLEMENTED")
        );
    }

    #[tokio::test]
    async fn silence_fails_at_the_deadline_and_says_it_was_published() {
        let (wallet, client, _pool) = harness_with_timeout(Duration::from_millis(300)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        let started = tokio::time::Instant::now();
        let err = client
            .make_invoice(1000, None, None)
            .await
            .expect_err("silence must not resolve");

        match err {
            NwcError::Timeout { published, .. } => {
                assert!(published, "the request did reach the relay");
                assert!(
                    err_may_reach(&NwcError::Timeout {
                        method: "x".into(),
                        timeout_ms: 1,
                        published: true
                    }),
                    "a published timeout must warn the caller"
                );
            }
            other => panic!("expected a Timeout, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    fn err_may_reach(e: &NwcError) -> bool {
        e.may_have_reached_wallet()
    }

    /// serde quotes the offending value, and a wallet can put an invoice there.
    #[tokio::test]
    async fn a_malformed_response_error_carries_no_payload() {
        let (wallet, client, _pool) = harness_with_timeout(Duration::from_millis(500)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Malformed)
            .await;

        let err = client.make_invoice(1000, None, None).await.unwrap_err();
        assert!(matches!(err, NwcError::MalformedResponse { .. }));
        let text = err.to_string();
        assert!(!text.contains("lnbc"), "{text}");
        assert!(text.contains("make_invoice"));
    }

    /// A wallet message can name the invoice, and the middlewares log errors.
    #[tokio::test]
    async fn an_invoice_in_a_wallet_message_never_reaches_the_error_text() {
        let (wallet, client, _pool) = harness().await;
        let invoice = "lnbc210n1pjqqqqqpp5abcdefghijklmnopqrstuvwxyz0123456789";
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Error {
                    code: "FAILED".to_string(),
                    message: format!("could not pay {invoice}"),
                },
            )
            .await;

        let env = client.pay_invoice("lnbc1", None).await.expect("call ok");
        let described = env.error.as_ref().unwrap().describe();
        assert!(!described.contains(invoice), "{described}");
        // And through the typed error a caller would surface.
        let wrapped = NwcError::Wallet {
            method: "pay_invoice".into(),
            detail: described,
        };
        assert!(!wrapped.to_string().contains("lnbc210n1pjqqqqq"));
    }

    // ── correlation, which is also an authorization check ────────────────────

    /// The author check is load-bearing: `nostr-relay-pool` verifies signatures
    /// but not filter matches, so a relay that ignores `authors` could deliver
    /// a stranger-signed, correctly correlated "settled".
    ///
    /// The old version of this test published before the client subscribed and
    /// without an `e` tag, so three independent layers dropped the event and
    /// deleting the author check still passed. This one correlates properly and
    /// is published only after a real request is in flight.
    #[tokio::test]
    async fn a_stranger_cannot_answer_even_when_correctly_correlated() {
        let (wallet, client, pool) = harness_with_timeout(Duration::from_millis(700)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;
        let client_pubkey = client.client_pubkey();

        let client = Arc::new(client);
        let caller = Arc::clone(&client);
        let call = tokio::spawn(async move { caller.make_invoice(1000, None, None).await });

        // Let the request be published so its id exists to correlate against.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let request_id = pool
            .stored_events()
            .await
            .into_iter()
            .find(|e| e.kind.as_u16() == 23194)
            .map(|e| e.id)
            .expect("the request was published");

        let stranger = Keys::generate();
        let plaintext =
            serde_json::json!({"result_type":"make_invoice","result":{"invoice":"evil"}})
                .to_string();
        let content =
            nip04::encrypt(stranger.secret_key(), &client_pubkey, plaintext).expect("encrypt");
        let evil = EventBuilder::new(Kind::from(23195u16), content)
            .tags([Tag::public_key(client_pubkey), Tag::event(request_id)])
            .sign_with_keys(&stranger)
            .expect("sign");
        // Delivered past the filter on purpose: a conformant relay would drop
        // this on `authors`, and then the client-side author check would never
        // be reached. The guard exists because `nostr-relay-pool` does not
        // verify that a delivered event matched the subscription.
        pool.publish_event_unfiltered(&evil).await;

        let result = call.await.expect("task");
        assert!(
            matches!(result, Err(NwcError::Timeout { .. })),
            "a stranger-signed answer must not satisfy the request, got {result:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_without_a_correlation_tag_is_ignored() {
        let (wallet, client, _pool) = harness_with_timeout(Duration::from_millis(500)).await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::NoCorrelationTag(invoice_result("uncorrelated")),
            )
            .await;
        assert!(matches!(
            client.make_invoice(1000, None, None).await,
            Err(NwcError::Timeout { .. })
        ));
    }

    /// Needs the mock to answer out of order, which requires per-request
    /// spawning: an inline wallet is strictly FIFO and this can never fail.
    #[tokio::test]
    async fn a_fast_answer_overtaking_a_slow_one_still_correlates() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Delayed {
                    delay: Duration::from_millis(300),
                    result: invoice_result("slow-make"),
                },
            )
            .await;
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::json!({ "preimage": "fast-pay" })),
            )
            .await;

        let client = Arc::new(client);
        let c1 = Arc::clone(&client);
        let c2 = Arc::clone(&client);

        let slow = tokio::spawn(async move { c1.make_invoice(1000, None, None).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let fast = c2.pay_invoice("lnbc1", None).await;

        // The fast answer lands while the slow one is still outstanding.
        assert_eq!(
            fast.expect("pay")
                .into_result()
                .expect("ok")
                .preimage
                .as_deref(),
            Some("fast-pay")
        );
        assert_eq!(
            slow.await
                .expect("task")
                .expect("make")
                .into_result()
                .expect("ok")
                .invoice
                .as_deref(),
            Some("slow-make"),
            "the slow request must not receive the fast answer"
        );
    }

    /// A mislabelled `result_type` is tolerated: correlation by event id
    /// already proves which request this answers.
    #[tokio::test]
    async fn a_mislabelled_result_type_is_tolerated() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::MislabelledResultType {
                    result_type: "something_else".to_string(),
                    result: invoice_result("still-mine"),
                },
            )
            .await;
        let env = client.make_invoice(1000, None, None).await.expect("ok");
        assert_eq!(
            env.into_result().unwrap().invoice.as_deref(),
            Some("still-mine")
        );
    }

    // ── lifecycle ────────────────────────────────────────────────────────────

    /// A cancelled request must not leave its waiter behind. The payment
    /// processor races every verification against a cancellation token, so a
    /// leak here is per-payment.
    #[tokio::test]
    async fn dropping_a_request_future_removes_its_waiter() {
        let (wallet, client, _pool) = harness_with_timeout(Duration::from_secs(5)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        {
            let fut = client.make_invoice(1000, None, None);
            // Drive it far enough to register the waiter, then drop it.
            assert!(
                tokio::time::timeout(Duration::from_millis(150), fut)
                    .await
                    .is_err(),
                "the silent wallet should not answer"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            client.waiter_count(),
            0,
            "a dropped request future must not orphan its waiter"
        );
    }

    /// `close()` must be fail-fast: before this, a closed client still
    /// published `pay_invoice` and the wallet still paid.
    #[tokio::test]
    async fn a_closed_client_refuses_to_publish() {
        let (wallet, client, _pool) = harness_with_timeout(Duration::from_millis(600)).await;
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::json!({ "preimage": "p" })),
            )
            .await;

        client.close();
        assert!(client.is_closed());

        let err = client
            .pay_invoice("lnbc1", None)
            .await
            .expect_err("a closed client must refuse");
        assert!(matches!(err, NwcError::Closed), "got {err:?}");
        assert!(!err.may_have_reached_wallet());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            wallet.call_count("pay_invoice").await,
            0,
            "a closed client must not cause a payment"
        );
    }

    #[tokio::test]
    async fn a_zero_response_timeout_is_refused_at_construction() {
        let pool = Arc::new(MockRelayPool::new());
        let secret = SecretKey::generate();
        let uri = format!(
            "nostr+walletconnect://{}?relay=wss%3A%2F%2Fmock.relay&secret={}",
            Keys::generate().public_key().to_hex(),
            secret.to_secret_hex()
        );
        let connection = parse_nwc_uri(&uri).unwrap();
        assert!(matches!(
            NwcClient::with_pool(
                pool as Arc<dyn RelayPoolTrait>,
                connection,
                NwcClientOptions::new().with_response_timeout(Duration::ZERO),
            ),
            Err(NwcError::Config { .. })
        ));
    }

    /// One panicking sink is caller code; it must not take the rail down.
    #[tokio::test]
    async fn a_panicking_sink_does_not_kill_the_rail() {
        let (wallet, client, _pool) = harness().await;
        client
            .on_notification(|_| panic!("a badly behaved sink"))
            .await
            .expect("sink registers");

        wallet
            .settle(&client.client_pubkey(), "hash-boom")
            .await
            .expect("notification published");
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(!client.is_closed(), "the reader must have survived");

        // And the rail still works.
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(invoice_result("after-panic")),
            )
            .await;
        let env = client
            .make_invoice(1000, None, None)
            .await
            .expect("the rail still answers after a sink panicked");
        assert_eq!(
            env.into_result().unwrap().invoice.as_deref(),
            Some("after-panic")
        );
    }

    // ── notifications ────────────────────────────────────────────────────────

    async fn collect_notifications(
        client: &NwcClient,
    ) -> tokio::sync::mpsc::UnboundedReceiver<NwcNotificationPayload> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        client
            .on_notification(move |p| {
                let _ = tx.send(p);
            })
            .await
            .expect("sink registers");
        rx
    }

    #[tokio::test]
    async fn decodes_a_nip04_notification() {
        let (wallet, client, _pool) = harness().await;
        let mut rx = collect_notifications(&client).await;
        wallet
            .settle(&client.client_pubkey(), "hash-04")
            .await
            .unwrap();
        let p = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("arrives")
            .expect("open");
        assert_eq!(p.notification_type, NOTIFICATION_PAYMENT_RECEIVED);
        assert_eq!(p.payment_hash(), Some("hash-04"));
    }

    #[tokio::test]
    async fn decodes_a_nip44_notification() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_quirks(MockWalletQuirks {
                nip44_notifications: true,
                ..Default::default()
            })
            .await;
        let mut rx = collect_notifications(&client).await;
        wallet
            .settle(&client.client_pubkey(), "hash-44")
            .await
            .unwrap();
        let p = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("arrives")
            .expect("open");
        assert_eq!(p.payment_hash(), Some("hash-44"));
    }

    /// ts's own fixture: NIP-04 content on kind 23197. A decrypt-by-kind
    /// client drops this.
    #[tokio::test]
    async fn decodes_nip04_content_published_on_the_nip44_kind() {
        let (wallet, client, _pool) = harness().await;
        let mut rx = collect_notifications(&client).await;
        wallet
            .settle_nip04_on_nip44_kind(&client.client_pubkey(), "hash-mixed")
            .await
            .unwrap();
        let p = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("decrypt by shape must handle NIP-04 on kind 23197")
            .expect("open");
        assert_eq!(p.payment_hash(), Some("hash-mixed"));
    }

    // ── info event ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetches_info_notification_types() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .publish_info_event(&["payment_received", "payment_sent"])
            .await
            .unwrap();
        let types = client.fetch_info_notification_types().await.unwrap();
        assert!(types.iter().any(|t| t == "payment_received"));
        assert!(types.iter().any(|t| t == "payment_sent"));
    }

    #[tokio::test]
    async fn missing_info_event_yields_empty_not_an_error() {
        let (_w, client, _p) = harness_with_timeout(Duration::from_millis(400)).await;
        assert!(client
            .fetch_info_notification_types()
            .await
            .expect("absence is not a failure")
            .is_empty());
    }
}
