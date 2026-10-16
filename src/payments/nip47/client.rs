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
//! [`crate::relay::RelayPool`], `connect` *adds* the wallet's relays
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

use crate::payments::nip47::error::{Delivery, NwcError};
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
    /// Latch for [`NwcClient::widen_response_filter_once`]. The pool has no
    /// unsubscribe, so the widened subscription may only ever be opened once.
    widened: AtomicBool,
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
                widened: AtomicBool::new(false),
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
    /// Requires the `p` tag addressed to us, which every conformant wallet
    /// sets. `since` keeps a relay that retains ephemeral events from
    /// replaying this client's whole history on every start and reconnect.
    fn filters(&self) -> Vec<Filter> {
        let since = Timestamp::now() - SUBSCRIPTION_LOOKBACK;
        vec![Filter::new()
            .kinds([
                Kind::from(KIND_RESPONSE),
                Kind::from(KIND_NOTIFICATION_LEGACY),
                Kind::from(KIND_NOTIFICATION),
            ])
            .author(self.connection.wallet_pubkey)
            .pubkey(self.keys.public_key())
            .since(since)]
    }

    /// The widened filter: responses only, without the `p` tag.
    ///
    /// Deliberately narrower than dropping `#p` everywhere. Only a response
    /// can prove the narrow filter is wrong, and leaving `#p` on the two
    /// notification kinds keeps the permanent subscription from picking up
    /// every notification the wallet sends any of its clients.
    fn widened_response_filter(&self) -> Vec<Filter> {
        vec![Filter::new()
            .kind(Kind::from(KIND_RESPONSE))
            .author(self.connection.wallet_pubkey)
            .since(Timestamp::now() - SUBSCRIPTION_LOOKBACK)]
    }

    /// Connect, subscribe, and spawn the reader task. Idempotent.
    async fn ensure_started(&self) -> Result<(), NwcError> {
        if self.is_closed() {
            return Err(NwcError::Closed {
                delivery: Delivery::NotSent,
            });
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
            .subscribe(self.filters())
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

    /// Re-subscribe to responses without the `p` tag, at most once ever.
    ///
    /// Our filter requires `#p`, which every conformant wallet sets but which
    /// is stricter than ts (whose per-request REQ has no `#p` at all). A wallet
    /// that omits it would be invisible forever.
    ///
    /// Two conditions, both necessary. Nothing has ever correlated, so the
    /// narrow filter has never been shown to work; and the caller has just
    /// waited out a full deadline on a request that *did* reach a relay, so it
    /// has now been shown to fail. Widening on "nothing has arrived yet" alone
    /// would re-subscribe a conformant wallet's client before its first answer
    /// could possibly land.
    ///
    /// The latch is what bounds it. [`RelayPoolTrait`] has no unsubscribe, so
    /// every widened subscription is permanent; without the latch a wallet
    /// that never answers would accumulate one per call.
    async fn widen_response_filter_once(&self) {
        if self.inner.ever_correlated.load(Ordering::SeqCst) {
            return;
        }
        if self.inner.widened.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::warn!(
            target: LOG_TARGET,
            "no NWC response has ever correlated; re-subscribing to responses              without the p-tag filter. The wallet is not setting the p tag it              is required to set."
        );
        if let Err(e) = self.pool.subscribe(self.widened_response_filter()).await {
            tracing::warn!(target: LOG_TARGET, error = %e, "widened NWC subscribe failed");
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
            return Err(NwcError::Closed {
                delivery: Delivery::NotSent,
            });
        }

        match tokio::time::timeout_at(deadline, self.ensure_started()).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(NwcError::Timeout {
                    method: method.to_string(),
                    timeout_ms,
                    delivery: Delivery::NotSent,
                })
            }
        }

        // Re-check: `close()` returns as soon as it has set the flag, so one
        // that lands during start-up would otherwise still be followed by a
        // publish. Nothing has gone out yet at this point.
        if self.is_closed() {
            return Err(NwcError::Closed {
                delivery: Delivery::NotSent,
            });
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
            let mut map = self.inner.waiters.lock().map_err(|_| NwcError::Closed {
                delivery: Delivery::NotSent,
            })?;
            map.insert(request_id, tx);
        }
        let _guard = WaiterGuard {
            inner: Arc::clone(&self.inner),
            id: request_id,
        };

        let delivery =
            match tokio::time::timeout_at(deadline, self.pool.publish_event(&event)).await {
                Ok(Ok(_)) => Delivery::Sent,
                // A relay that answered with an error did not accept the
                // event, so this one really is a non-delivery.
                Ok(Err(e)) => {
                    return Err(NwcError::Publish {
                        method: method.to_string(),
                        reason: e.to_string(),
                    })
                }
                // The deadline fell inside the publish. The pool writes the
                // EVENT before it waits on the relay's OK, so the wallet may
                // already have the request: this is unknown, not "never sent".
                Err(_) => {
                    return Err(NwcError::Timeout {
                        method: method.to_string(),
                        timeout_ms,
                        delivery: Delivery::Unknown,
                    })
                }
            };

        tracing::debug!(
            target: LOG_TARGET,
            method,
            request_id = %request_id.to_hex(),
            "NWC request published"
        );

        let plaintext = match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(p)) => p,
            // Sender dropped: the client closed, or the reader died. The
            // request is already out, so the delivery bit has to survive:
            // a `close()` after a `pay_invoice` is not a safe retry.
            Ok(Err(_)) => return Err(NwcError::Closed { delivery }),
            Err(_) => {
                // The narrow filter has now demonstrably failed to deliver an
                // answer to a request we know went out, which is the only
                // evidence that would justify widening it.
                self.widen_response_filter_once().await;
                return Err(NwcError::Timeout {
                    method: method.to_string(),
                    timeout_ms,
                    delivery,
                });
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
                    // Wallet-controlled, so it is scrubbed like any other
                    // wallet string before it reaches a log line.
                    result_type = %sanitize_wallet_text(kind),
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
    /// the shared request path.
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
    /// the shared request path.
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
    /// See the shared request path. On [`NwcError::Timeout`], check
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

    /// A pool that delegates to [`MockRelayPool`] while recording every
    /// `subscribe` and optionally parking `connect`.
    ///
    /// Recording the filters lets the widening tests assert what the
    /// subscriptions *match*, rather than their shape. Parking `connect` is
    /// the only way to reach the window between the top-of-request closed
    /// check and the publish.
    struct SpyPool {
        inner: Arc<MockRelayPool>,
        connect_delay: Duration,
        publish_delay: Duration,
        post_publish_delay: Duration,
        fail_publish: bool,
        subscribed: std::sync::Mutex<Vec<Vec<Filter>>>,
        /// The spy owns the stream the client reads, so a test can overflow
        /// it (to force `Lagged`) or close it (to force a reader exit).
        relay_tx: std::sync::Mutex<Option<tokio::sync::broadcast::Sender<RelayPoolNotification>>>,
        forwarder: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    }

    impl SpyPool {
        fn new(inner: Arc<MockRelayPool>, connect_delay: Duration) -> Self {
            Self::with_capacity(inner, connect_delay, 1024)
        }

        fn with_capacity(
            inner: Arc<MockRelayPool>,
            connect_delay: Duration,
            capacity: usize,
        ) -> Self {
            let (relay_tx, _) = tokio::sync::broadcast::channel(capacity);
            // Re-broadcast the real pool's stream through our own channel.
            let mut upstream = inner.notifications();
            let tx = relay_tx.clone();
            let forwarder = tokio::spawn(async move {
                while let Ok(n) = upstream.recv().await {
                    if tx.send(n).is_err() {
                        break;
                    }
                }
            });
            Self {
                inner,
                connect_delay,
                publish_delay: Duration::ZERO,
                post_publish_delay: Duration::ZERO,
                fail_publish: false,
                subscribed: std::sync::Mutex::new(Vec::new()),
                relay_tx: std::sync::Mutex::new(Some(relay_tx)),
                forwarder: std::sync::Mutex::new(Some(forwarder)),
            }
        }

        fn with_publish_delay(mut self, delay: Duration) -> Self {
            self.publish_delay = delay;
            self
        }

        /// Hold the caller inside `publish_event` *after* the event is on the
        /// wire, so the answer provably lands before `publish` returns.
        fn with_post_publish_delay(mut self, delay: Duration) -> Self {
            self.post_publish_delay = delay;
            self
        }

        fn with_failing_publish(mut self) -> Self {
            self.fail_publish = true;
            self
        }

        fn subscriptions(&self) -> Vec<Vec<Filter>> {
            self.subscribed.lock().expect("not poisoned").clone()
        }

        /// Overflow the client's receiver. Called synchronously on a
        /// current-thread runtime, the reader cannot drain in between, so it
        /// is guaranteed to observe `Lagged`.
        fn flood(&self, events: usize, filler: &Event) {
            let guard = self.relay_tx.lock().expect("not poisoned");
            let Some(tx) = guard.as_ref() else { return };
            for _ in 0..events {
                let _ = tx.send(RelayPoolNotification::Event {
                    relay_url: RelayUrl::parse("wss://mock.relay").expect("url"),
                    subscription_id: SubscriptionId::new("spy"),
                    event: Box::new(filler.clone()),
                });
            }
        }

        /// Close the stream the reader is sitting on, as a dropped pool does.
        ///
        /// Both senders have to go: aborting the forwarder alone leaves our
        /// own clone alive, and the receiver then simply waits forever rather
        /// than seeing the channel close.
        fn kill_stream(&self) {
            if let Some(handle) = self.forwarder.lock().expect("not poisoned").take() {
                handle.abort();
            }
            drop(self.relay_tx.lock().expect("not poisoned").take());
        }
    }

    #[async_trait::async_trait]
    impl RelayPoolTrait for SpyPool {
        async fn connect(&self, relay_urls: &[String]) -> crate::Result<()> {
            if !self.connect_delay.is_zero() {
                tokio::time::sleep(self.connect_delay).await;
            }
            self.inner.connect(relay_urls).await
        }
        async fn disconnect(&self) -> crate::Result<()> {
            self.inner.disconnect().await
        }
        async fn publish_event(&self, event: &Event) -> crate::Result<EventId> {
            if !self.publish_delay.is_zero() {
                tokio::time::sleep(self.publish_delay).await;
            }
            if self.fail_publish {
                return Err(crate::Error::Transport("injected publish failure".into()));
            }
            let id = self.inner.publish_event(event).await;
            if !self.post_publish_delay.is_zero() {
                tokio::time::sleep(self.post_publish_delay).await;
            }
            id
        }
        async fn publish(&self, builder: EventBuilder) -> crate::Result<EventId> {
            self.inner.publish(builder).await
        }
        async fn sign(&self, builder: EventBuilder) -> crate::Result<Event> {
            self.inner.sign(builder).await
        }
        async fn signer(&self) -> crate::Result<Arc<dyn NostrSigner>> {
            self.inner.signer().await
        }
        fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
            self.relay_tx
                .lock()
                .expect("not poisoned")
                .as_ref()
                .expect("stream not killed")
                .subscribe()
        }
        async fn public_key(&self) -> crate::Result<PublicKey> {
            self.inner.public_key().await
        }
        async fn subscribe(&self, filters: Vec<Filter>) -> crate::Result<()> {
            self.subscribed
                .lock()
                .expect("not poisoned")
                .push(filters.clone());
            self.inner.subscribe(filters).await
        }
        async fn publish_to(
            &self,
            urls: &[String],
            builder: EventBuilder,
        ) -> crate::Result<EventId> {
            self.inner.publish_to(urls, builder).await
        }
        async fn fetch_events(
            &self,
            filters: Vec<Filter>,
            timeout: Duration,
        ) -> crate::Result<Vec<Event>> {
            self.inner.fetch_events(filters, timeout).await
        }
    }

    /// Harness whose client talks through a [`SpyPool`].
    async fn spy_harness(
        response_timeout: Duration,
        connect_delay: Duration,
    ) -> (Arc<MockWallet>, NwcClient, Arc<SpyPool>) {
        spy_harness_with(response_timeout, |pool| SpyPool::new(pool, connect_delay)).await
    }

    async fn spy_harness_with(
        response_timeout: Duration,
        build: impl FnOnce(Arc<MockRelayPool>) -> SpyPool,
    ) -> (Arc<MockWallet>, NwcClient, Arc<SpyPool>) {
        let wallet_pool = Arc::new(MockRelayPool::new());
        let client_pool = Arc::new(wallet_pool.linked_with_keys(Keys::generate()));
        let spy = Arc::new(build(client_pool));

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
            Arc::clone(&spy) as Arc<dyn RelayPoolTrait>,
            connection,
            NwcClientOptions::new().with_response_timeout(response_timeout),
        )
        .expect("options valid");
        (wallet, client, spy)
    }

    fn invoice_result(invoice: &str) -> serde_json::Value {
        serde_json::json!({ "invoice": invoice, "state": "pending" })
    }

    // ── (3) widening, and (24) close ─────────────────────────────────────────

    /// Build a wallet-signed event with no `p` tag, as a non-conformant
    /// wallet emits, so a filter can be asked what it would match.
    fn untagged_from(wallet: &MockWallet, kind: u16) -> Event {
        EventBuilder::new(Kind::from(kind), "ciphertext")
            .sign_with_keys(&wallet.keys())
            .expect("signs")
    }

    /// (3). Widening must not run until the narrow filter has *failed*.
    /// Running it right after the first publish re-subscribes a conformant
    /// wallet's client before its first answer could possibly have landed.
    #[tokio::test]
    async fn a_conformant_wallet_never_triggers_the_widening() {
        let (wallet, client, spy) = spy_harness(Duration::from_secs(5), Duration::ZERO).await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(invoice_result("lnbc210n1p")),
            )
            .await;

        client
            .make_invoice(1000, None, None)
            .await
            .expect("conformant wallet answers");

        assert_eq!(
            spy.subscriptions().len(),
            1,
            "a wallet that answers must never be re-subscribed"
        );
    }

    /// (3). The pool has no unsubscribe, so every widened subscription is
    /// permanent. A wallet that never answers must still produce exactly one.
    #[tokio::test]
    async fn widening_happens_at_most_once_however_many_requests_fail() {
        let (wallet, client, spy) = spy_harness(Duration::from_millis(150), Duration::ZERO).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        for _ in 0..4 {
            let _ = client.make_invoice(1000, None, None).await;
        }

        let subs = spy.subscriptions();
        assert_eq!(
            subs.len(),
            2,
            "one initial narrow subscribe and exactly one widened, got {}",
            subs.len()
        );
    }

    /// (3). Only the response filter loses `#p`. Dropping it from the two
    /// notification kinds as well would make the permanent subscription pick
    /// up every notification the wallet sends any of its clients.
    #[tokio::test]
    async fn the_widened_filter_covers_responses_only() {
        let (wallet, client, spy) = spy_harness(Duration::from_millis(150), Duration::ZERO).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;
        let _ = client.make_invoice(1000, None, None).await;

        let subs = spy.subscriptions();
        assert_eq!(subs.len(), 2, "expected a widened subscribe");
        let widened = &subs[1];

        let response = untagged_from(&wallet, KIND_RESPONSE);
        assert!(
            widened
                .iter()
                .any(|f| f.match_event(&response, MatchEventOptions::default())),
            "an untagged response must now match"
        );

        for kind in [KIND_NOTIFICATION_LEGACY, KIND_NOTIFICATION] {
            let notification = untagged_from(&wallet, kind);
            assert!(
                !widened
                    .iter()
                    .any(|f| f.match_event(&notification, MatchEventOptions::default())),
                "kind {kind} must keep its p-tag requirement"
            );
        }
    }

    /// (3). The point of the whole mechanism: a wallet that never sets `p` is
    /// invisible to the narrow filter, and must become visible afterwards.
    #[tokio::test]
    async fn a_wallet_that_omits_the_p_tag_is_recovered_after_one_failure() {
        let (wallet, client, _spy) = spy_harness(Duration::from_millis(400), Duration::ZERO).await;
        wallet
            .set_quirks(MockWalletQuirks {
                omit_p_tag: true,
                ..Default::default()
            })
            .await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(invoice_result("lnbc210n1p")),
            )
            .await;

        let first = client.make_invoice(1000, None, None).await;
        assert!(
            matches!(first, Err(NwcError::Timeout { .. })),
            "the narrow filter cannot see this wallet: {first:?}"
        );

        client
            .make_invoice(1000, None, None)
            .await
            .expect("the widened filter must now see the answer");
    }

    /// (8a). The deadline falling inside the publish is unknowable, not a
    /// definite non-delivery: the pool writes the EVENT before it waits on
    /// the relay's OK. Reporting `NotSent` here tells a `pay_invoice` caller
    /// that retrying is safe when it may not be.
    #[tokio::test]
    async fn a_deadline_inside_the_publish_reports_unknown_delivery() {
        let (wallet, client, _spy) = spy_harness_with(Duration::from_millis(120), |pool| {
            SpyPool::new(pool, Duration::ZERO).with_publish_delay(Duration::from_millis(600))
        })
        .await;
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::json!({ "preimage": "deadbeef" })),
            )
            .await;

        let err = client
            .pay_invoice("lnbc210n1p", None)
            .await
            .expect_err("the deadline falls inside the publish");

        match err {
            NwcError::Timeout { delivery, .. } => assert_eq!(
                delivery,
                Delivery::Unknown,
                "an in-flight publish is unknown, not a non-delivery"
            ),
            other => panic!("expected a Timeout, got {other:?}"),
        }
        assert!(
            err_may_reach(&err),
            "the caller must not be told this is a safe retry"
        );
    }

    /// (24). A `close()` landing after the request went out must keep the
    /// delivery bit. Collapsing it to a bare `Closed` tells the caller a
    /// `pay_invoice` that is already on the wire is safe to retry.
    #[tokio::test]
    async fn a_close_after_the_publish_keeps_the_delivery_bit() {
        let (wallet, client, _spy) = spy_harness(Duration::from_secs(5), Duration::ZERO).await;
        wallet
            .set_behavior("pay_invoice", WalletBehavior::Silence)
            .await;

        let client = Arc::new(client);
        let task = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.pay_invoice("lnbc210n1p", None).await })
        };

        // Long enough for the publish to have completed and the wait to have
        // begun, well short of the 5 s deadline.
        tokio::time::sleep(Duration::from_millis(150)).await;
        client.close();

        let err = task
            .await
            .expect("task joins")
            .expect_err("closing releases the waiter");
        assert!(
            matches!(
                err,
                NwcError::Closed {
                    delivery: Delivery::Sent
                }
            ),
            "expected a sent Closed, got {err:?}"
        );
        assert!(
            err_may_reach(&err),
            "the wallet has the pay_invoice: this is not a safe retry"
        );
    }

    /// (24). `close()` returns as soon as the flag is set, so one that lands
    /// while start-up is still in flight used to be followed by a publish.
    #[tokio::test]
    async fn a_close_during_start_up_still_prevents_the_publish() {
        let (wallet, client, _spy) =
            spy_harness(Duration::from_secs(5), Duration::from_millis(300)).await;
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::json!({ "preimage": "deadbeef" })),
            )
            .await;

        let client = Arc::new(client);
        let task = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.pay_invoice("lnbc210n1p", None).await })
        };

        // Inside the parked connect, past the top-of-request closed check.
        tokio::time::sleep(Duration::from_millis(60)).await;
        client.close();

        let err = task
            .await
            .expect("task joins")
            .expect_err("a closed client must not pay");
        assert!(
            matches!(
                err,
                NwcError::Closed {
                    delivery: Delivery::NotSent
                }
            ),
            "expected a not-sent Closed, got {err:?}"
        );
        assert!(
            wallet.calls().await.is_empty(),
            "the wallet must not have been asked to pay: {:?}",
            wallet.calls().await
        );
    }

    // GUARDS_ON_THE_MONEY_PATH

    /// A lagging broadcast must not kill the reader. If it breaks the loop,
    /// the exit path marks the client closed and every later request fails,
    /// while requests already in flight can still have paid.
    #[tokio::test]
    async fn a_lagging_broadcast_does_not_kill_the_reader() {
        let (wallet, client, spy) = spy_harness_with(Duration::from_secs(5), |pool| {
            SpyPool::with_capacity(pool, Duration::ZERO, 4)
        })
        .await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(invoice_result("lnbc210n1p")),
            )
            .await;

        client.make_invoice(1000, None, None).await.expect("first");

        // Synchronous on a current-thread runtime: the reader cannot drain
        // between sends, so it is certain to observe `Lagged`.
        let filler = untagged_from(&wallet, KIND_RESPONSE);
        spy.flood(32, &filler);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!client.is_closed(), "lagging must not close the client");
        client
            .make_invoice(1000, None, None)
            .await
            .expect("the reader must still be alive after lagging");
    }

    /// The waiter is registered before the publish so an instant answer
    /// cannot arrive before anything is listening. Holding the caller inside
    /// `publish_event` after the event is on the wire makes that window
    /// deterministic rather than a race the test usually wins.
    #[tokio::test]
    async fn an_answer_arriving_during_the_publish_is_not_lost() {
        let (wallet, client, _spy) = spy_harness_with(Duration::from_millis(900), |pool| {
            SpyPool::new(pool, Duration::ZERO).with_post_publish_delay(Duration::from_millis(300))
        })
        .await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(invoice_result("lnbc210n1p")),
            )
            .await;

        client
            .make_invoice(1000, None, None)
            .await
            .expect("an answer landing during the publish must still correlate");
    }

    /// The guard removes the waiter on every exit, not only the happy one.
    /// A leak is invisible from outside and accumulates one `EventId` and one
    /// `oneshot::Sender` per cancelled verification.
    #[tokio::test]
    async fn the_waiter_is_removed_when_the_request_times_out() {
        let (wallet, client, _spy) = spy_harness(Duration::from_millis(150), Duration::ZERO).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        let _ = client.make_invoice(1000, None, None).await;
        assert_eq!(client.waiter_count(), 0, "timeout path leaked a waiter");
    }

    #[tokio::test]
    async fn the_waiter_is_removed_when_the_publish_fails() {
        let (wallet, client, _spy) = spy_harness_with(Duration::from_secs(5), |pool| {
            SpyPool::new(pool, Duration::ZERO).with_failing_publish()
        })
        .await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        let err = client
            .make_invoice(1000, None, None)
            .await
            .expect_err("the publish fails");
        assert!(matches!(err, NwcError::Publish { .. }), "{err:?}");
        assert_eq!(
            client.waiter_count(),
            0,
            "publish-failure path leaked a waiter"
        );
    }

    /// The waiter is also dropped when the `request()` future is cancelled,
    /// which is what the payment processor's `select!` does on every
    /// cancelled verification.
    #[tokio::test]
    async fn the_waiter_is_removed_when_the_caller_is_cancelled() {
        let (wallet, client, _spy) = spy_harness(Duration::from_secs(30), Duration::ZERO).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        tokio::time::timeout(
            Duration::from_millis(200),
            client.make_invoice(1000, None, None),
        )
        .await
        .expect_err("the caller gives up first");

        assert_eq!(client.waiter_count(), 0, "a dropped future leaked a waiter");
    }

    /// When the reader exits it releases everyone waiting, rather than
    /// leaving them to sit out a deadline that can be minutes long.
    #[tokio::test]
    async fn a_reader_exit_releases_the_waiting_caller() {
        let (wallet, client, spy) = spy_harness(Duration::from_secs(30), Duration::ZERO).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        let client = Arc::new(client);
        let caller = Arc::clone(&client);
        let started = tokio::time::Instant::now();
        let call = tokio::spawn(async move { caller.make_invoice(1000, None, None).await });

        tokio::time::sleep(Duration::from_millis(120)).await;
        spy.kill_stream();

        let result = call.await.expect("task joins");
        assert!(
            matches!(result, Err(NwcError::Closed { .. })),
            "expected Closed, got {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the caller sat out the deadline instead of being released: {:?}",
            started.elapsed()
        );
        assert!(client.is_closed(), "a dead reader must close the client");
    }

    /// (15). Without `since`, a relay that retains ephemeral events replays
    /// this client's whole history on every start and reconnect, and the
    /// replayed notifications reach the sinks.
    #[tokio::test]
    async fn a_stale_notification_is_not_replayed_at_subscribe() {
        let (wallet, client, _spy) = spy_harness(Duration::from_secs(5), Duration::ZERO).await;
        let client_pubkey = client.client_pubkey();

        let stale = Timestamp::now() - (SUBSCRIPTION_LOOKBACK + Duration::from_secs(600));
        let payload = serde_json::json!({
            "notification_type": NOTIFICATION_PAYMENT_RECEIVED,
            "notification": { "invoice": "lnbc-old", "amount": 1000 }
        })
        .to_string();
        let content =
            nip04::encrypt(wallet.keys().secret_key(), &client_pubkey, payload).expect("encrypt");
        let old_event = EventBuilder::new(Kind::from(KIND_NOTIFICATION_LEGACY), content)
            .tags([Tag::public_key(client_pubkey)])
            .custom_created_at(stale)
            .sign_with_keys(&wallet.keys())
            .expect("sign");

        // Stored before the client ever subscribes, so only the filter decides
        // whether it is replayed.
        wallet.pool().inject_event(old_event.clone()).await;

        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        client
            .on_notification(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .expect("sink registers");

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "a stale notification must not be replayed"
        );
    }

    /// (18). The NIP-40 expiration is what stops a wallet acting on a
    /// `pay_invoice` the caller has already given up on.
    #[tokio::test]
    async fn pay_invoice_carries_the_nip40_expiration_when_asked() {
        let (wallet, client, _spy) = spy_harness(Duration::from_millis(250), Duration::ZERO).await;
        wallet
            .set_behavior("pay_invoice", WalletBehavior::Silence)
            .await;

        let _ = client
            .pay_invoice("lnbc210n1p", Some(Duration::from_secs(60)))
            .await;

        let request = wallet
            .pool()
            .stored_events()
            .await
            .into_iter()
            .find(|e| e.kind.as_u16() == KIND_REQUEST)
            .expect("the request was published");
        let expiration = request
            .tags
            .iter()
            .find_map(|t| match t.as_slice() {
                [name, value, ..] if name == "expiration" => value.parse::<u64>().ok(),
                _ => None,
            })
            .expect("a NIP-40 expiration tag");
        let now = Timestamp::now().as_secs();
        assert!(
            expiration > now && expiration <= now + 61,
            "expiration {expiration} is not about 60s out from {now}"
        );
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
            NwcError::Timeout { delivery, .. } => {
                assert_eq!(delivery, Delivery::Sent, "the request did reach the relay");
                assert!(
                    err_may_reach(&NwcError::Timeout {
                        method: "x".into(),
                        timeout_ms: 1,
                        delivery: Delivery::Sent
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

    /// Kept alongside `an_answer_without_a_correlation_tag_is_ignored`,
    /// which cannot see this: an answer with *no* `e` tag is dropped before
    /// the correlation lookup is reached, so a fallback that accepted a
    /// *mismatched* `e` tag would survive that test. This one carries a
    /// well-formed `e` tag pointing at the wrong request.
    #[tokio::test]
    async fn an_answer_correlated_to_a_different_request_is_ignored() {
        let (wallet, client, pool) = harness_with_timeout(Duration::from_millis(700)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;
        let client_pubkey = client.client_pubkey();

        let client = Arc::new(client);
        let caller = Arc::clone(&client);
        let call = tokio::spawn(async move { caller.make_invoice(1000, None, None).await });

        tokio::time::sleep(Duration::from_millis(120)).await;

        // A syntactically valid event id that is not the in-flight request's.
        let other_request = EventId::from_slice(&[7u8; 32]).expect("valid id");
        let plaintext =
            serde_json::json!({"result_type":"make_invoice","result":{"invoice":"wrong"}})
                .to_string();
        let content =
            nip04::encrypt(wallet.keys().secret_key(), &client_pubkey, plaintext).expect("encrypt");
        let misdirected = EventBuilder::new(Kind::from(KIND_RESPONSE), content)
            .tags([Tag::public_key(client_pubkey), Tag::event(other_request)])
            .sign_with_keys(&wallet.keys())
            .expect("sign");
        pool.publish_event_unfiltered(&misdirected).await;

        let result = call.await.expect("task");
        assert!(
            matches!(result, Err(NwcError::Timeout { .. })),
            "an answer for another request must not satisfy this one, got {result:?}"
        );
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
        assert!(
            matches!(
                err,
                NwcError::Closed {
                    delivery: Delivery::NotSent,
                }
            ),
            "got {err:?}"
        );
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
