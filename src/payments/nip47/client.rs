//! Minimal NIP-47 client: request/response and notifications over a
//! [`RelayPoolTrait`], with no dependency on the `nwc` crate.
//!
//! # Why not the `nwc` crate
//!
//! `nwc` owns its relay pool privately, so nothing built on it can be driven by
//! [`MockRelayPool`](crate::relay::MockRelayPool) and every test would need a
//! live wallet. This client takes an injected `Arc<dyn RelayPoolTrait>` instead,
//! so the same code path runs in CI against a scripted mock wallet and in
//! production against a real one.
//!
//! # One subscription, not one per request
//!
//! ts-sdk opens a subscription per request and closes it on settle.
//! [`RelayPoolTrait`] has no unsubscribe, so that shape would leak one live REQ
//! per payment for the life of the process. This client instead opens a single
//! subscription covering responses and notifications, runs one reader task, and
//! correlates in process through a waiter map keyed by the request event id.
//!
//! # Divergences from ts-sdk (both safe, neither visible on the wire)
//!
//! * Notifications are decrypted **by kind**: 23196 is NIP-04, 23197 is NIP-44.
//!   ts always uses NIP-04, which silently drops NIP-44 notifications.
//! * Requests are issued concurrently and correlated by id, where ts serializes
//!   them behind a promise queue.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::payments::errors::PaymentError;
use crate::payments::nip47::types::{NwcNotificationPayload, NwcResponseEnvelope};
use crate::payments::nip47::uri::NwcConnection;
use crate::relay::RelayPoolTrait;

const LOG_TARGET: &str = "contextvm::payments::nip47";

/// Kind 13194: the wallet service's info event.
const KIND_INFO: u16 = 13194;
/// Kind 23194: a client request to the wallet.
const KIND_REQUEST: u16 = 23194;
/// Kind 23195: the wallet's response.
const KIND_RESPONSE: u16 = 23195;
/// Kind 23196: a NIP-04-encrypted notification (legacy).
const KIND_NOTIFICATION_NIP04: u16 = 23196;
/// Kind 23197: a NIP-44-encrypted notification.
const KIND_NOTIFICATION_NIP44: u16 = 23197;

/// Default per-request response timeout.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// Construction options for [`NwcClient`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NwcClientOptions {
    /// How long to wait for a single NIP-47 response. Default 60 s.
    pub response_timeout: Duration,
}

impl Default for NwcClientOptions {
    fn default() -> Self {
        Self {
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
        }
    }
}

type Waiter = oneshot::Sender<String>;

/// A registered notification sink. Named so the waiter/sink collections stay
/// readable (and to satisfy `clippy::type_complexity`).
type NotificationSink = Arc<dyn Fn(NwcNotificationPayload) + Send + Sync>;

struct Inner {
    /// Pending request-event-id to response-plaintext waiters.
    waiters: Mutex<HashMap<EventId, Waiter>>,
    /// Registered notification sinks.
    notification_sinks: Mutex<Vec<NotificationSink>>,
}

/// A NIP-47 client bound to one wallet connection and one relay pool.
pub struct NwcClient {
    pool: Arc<dyn RelayPoolTrait>,
    connection: NwcConnection,
    keys: Keys,
    response_timeout: Duration,
    inner: Arc<Inner>,
    /// Guards one-time subscription + reader-task startup.
    started: Mutex<bool>,
    cancel: CancellationToken,
}

impl NwcClient {
    /// Create a client over `pool` for `connection`, with default options.
    pub fn new(pool: Arc<dyn RelayPoolTrait>, connection: NwcConnection) -> Self {
        Self::with_options(pool, connection, NwcClientOptions::default())
    }

    /// Create a client with explicit options.
    pub fn with_options(
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
                notification_sinks: Mutex::new(Vec::new()),
            }),
            started: Mutex::new(false),
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

    /// Stop the reader task. Idempotent; the client is unusable afterwards.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    /// Connect, subscribe, and spawn the reader task. Idempotent.
    async fn ensure_started(&self) -> Result<(), PaymentError> {
        let mut started = self.started.lock().await;
        if *started {
            return Ok(());
        }

        self.pool
            .connect(&self.connection.relay_urls())
            .await
            .map_err(|e| PaymentError::Processor(format!("NWC relay connect failed: {e}")))?;

        // One subscription for everything this client consumes: responses
        // addressed to us, and both notification kinds.
        let client_pk = self.keys.public_key();
        let wallet_pk = self.connection.wallet_pubkey;
        let filters = vec![Filter::new()
            .kinds([
                Kind::from(KIND_RESPONSE),
                Kind::from(KIND_NOTIFICATION_NIP04),
                Kind::from(KIND_NOTIFICATION_NIP44),
            ])
            .author(wallet_pk)
            .pubkey(client_pk)];

        // Take the receiver BEFORE subscribing so the mock pool's replay of
        // already-stored events cannot land before we are listening.
        let notifications = self.pool.notifications();

        self.pool
            .subscribe(filters)
            .await
            .map_err(|e| PaymentError::Processor(format!("NWC subscribe failed: {e}")))?;

        let inner = Arc::clone(&self.inner);
        let keys = self.keys.clone();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            Self::reader_loop(notifications, inner, keys, wallet_pk, cancel).await;
        });

        *started = true;
        Ok(())
    }

    /// Single reader task: decrypts, correlates responses, fans out notifications.
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
                    // A lagged broadcast drops events but must not kill the
                    // reader: the waiter map survives, so an in-flight request
                    // still resolves from a later delivery or times out
                    // cleanly. Killing the loop here would hang every waiter.
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

            // A stranger cannot answer for the wallet.
            if event.pubkey != wallet_pubkey {
                continue;
            }

            let kind = event.kind.as_u16();
            match kind {
                KIND_RESPONSE => Self::handle_response(&event, &inner, &keys).await,
                KIND_NOTIFICATION_NIP04 | KIND_NOTIFICATION_NIP44 => {
                    Self::handle_notification(&event, &inner, &keys, kind).await
                }
                _ => {}
            }
        }
        tracing::debug!(target: LOG_TARGET, "NWC reader loop stopped");
    }

    async fn handle_response(event: &Event, inner: &Arc<Inner>, keys: &Keys) {
        // Correlate by the `e` tag naming the request event.
        let Some(request_id) = first_e_tag(event) else {
            return;
        };

        let waiter = {
            let mut waiters = inner.waiters.lock().await;
            waiters.remove(&request_id)
        };
        let Some(waiter) = waiter else {
            // Uncorrelated: a response to a request we never made, or one we
            // already settled. Dropping it is correct.
            tracing::debug!(
                target: LOG_TARGET,
                request_id = %request_id.to_hex(),
                "NWC response did not correlate to a pending request"
            );
            return;
        };

        // Responses are NIP-04 in practice; fall back to NIP-44 so a wallet
        // that upgrades does not strand us.
        match decrypt_either(keys, &event.pubkey, &event.content) {
            Ok(plaintext) => {
                let _ = waiter.send(plaintext);
            }
            Err(e) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    error = %e,
                    "failed to decrypt NWC response; dropping (caller will time out)"
                );
            }
        }
    }

    async fn handle_notification(event: &Event, inner: &Arc<Inner>, keys: &Keys, kind: u16) {
        // Decrypt by kind, the documented divergence from ts-sdk.
        let decrypted = if kind == KIND_NOTIFICATION_NIP44 {
            nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content)
                .map_err(|e| e.to_string())
        } else {
            nip04::decrypt(keys.secret_key(), &event.pubkey, &event.content)
                .map_err(|e| e.to_string())
        };

        let Ok(plaintext) = decrypted else {
            tracing::debug!(
                target: LOG_TARGET,
                kind,
                "failed to decrypt NWC notification"
            );
            return;
        };

        let Ok(payload) = serde_json::from_str::<NwcNotificationPayload>(&plaintext) else {
            tracing::debug!(target: LOG_TARGET, "malformed NWC notification payload");
            return;
        };

        let sinks = {
            let guard = inner.notification_sinks.lock().await;
            guard.clone()
        };
        for sink in sinks {
            sink(payload.clone());
        }
    }

    /// Register a sink invoked for every decoded notification.
    ///
    /// Sinks are called synchronously on the reader task, so they must not
    /// block. Kept for the life of the client; there is no removal, matching
    /// the single long-lived subscriber this rail needs.
    pub async fn on_notification<F>(&self, sink: F) -> Result<(), PaymentError>
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
    /// Bounded by `response_timeout`. A wallet-reported error is returned as
    /// `Ok(envelope)` with `error` set, not `Err`: the caller decides whether a
    /// given code is fatal or merely pending.
    ///
    /// # Errors
    ///
    /// [`PaymentError::Processor`] on encrypt/publish failure, on timeout, or
    /// when the response cannot be parsed.
    pub async fn request<P, R>(
        &self,
        method: &str,
        params: P,
    ) -> Result<NwcResponseEnvelope<R>, PaymentError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        self.ensure_started().await?;

        let body = serde_json::json!({ "method": method, "params": params });
        let plaintext = serde_json::to_string(&body)?;

        let content = nip04::encrypt(
            self.keys.secret_key(),
            &self.connection.wallet_pubkey,
            plaintext,
        )
        .map_err(|e| PaymentError::Processor(format!("NWC request encryption failed: {e}")))?;

        let builder = EventBuilder::new(Kind::from(KIND_REQUEST), content).tags([
            Tag::public_key(self.connection.wallet_pubkey),
            Tag::parse(["encryption", "nip04"])
                .map_err(|e| PaymentError::Processor(format!("bad encryption tag: {e}")))?,
        ]);

        let event = builder
            .sign_with_keys(&self.keys)
            .map_err(|e| PaymentError::Processor(format!("NWC request signing failed: {e}")))?;
        let request_id = event.id;

        // Register the waiter BEFORE publishing, so a wallet that answers
        // instantly cannot arrive before we are listening for it.
        let (tx, rx) = oneshot::channel();
        self.inner.waiters.lock().await.insert(request_id, tx);

        if let Err(e) = self.pool.publish_event(&event).await {
            self.inner.waiters.lock().await.remove(&request_id);
            return Err(PaymentError::Processor(format!(
                "NWC publish failed for {method}: {e}"
            )));
        }

        tracing::debug!(
            target: LOG_TARGET,
            method,
            request_id = %request_id.to_hex(),
            "NWC request published"
        );

        let plaintext = match tokio::time::timeout(self.response_timeout, rx).await {
            Ok(Ok(p)) => p,
            Ok(Err(_)) => {
                self.inner.waiters.lock().await.remove(&request_id);
                return Err(PaymentError::Processor(format!(
                    "NWC client stopped before a response to {method} arrived"
                )));
            }
            Err(_) => {
                self.inner.waiters.lock().await.remove(&request_id);
                return Err(PaymentError::Processor(format!(
                    "NWC response timed out for {method}"
                )));
            }
        };

        let envelope: NwcResponseEnvelope<R> = serde_json::from_str(&plaintext).map_err(|e| {
            PaymentError::Processor(format!("malformed NWC response for {method}: {e}"))
        })?;

        Ok(envelope)
    }

    /// Fetch the notification types the wallet advertises on its kind-13194 info event.
    ///
    /// Returns an empty set when no info event is found, which callers read as
    /// "no notification support" and fall back to polling.
    pub async fn fetch_info_notification_types(&self) -> Result<Vec<String>, PaymentError> {
        self.ensure_started().await?;

        let filter = Filter::new()
            .kind(Kind::from(KIND_INFO))
            .author(self.connection.wallet_pubkey)
            .limit(1);

        let events = self
            .pool
            .fetch_events(vec![filter], self.response_timeout)
            .await
            .map_err(|e| PaymentError::Processor(format!("NWC info fetch failed: {e}")))?;

        let Some(event) = events.into_iter().next() else {
            return Ok(Vec::new());
        };

        // ts joins every `notifications` tag value with a space, then splits on
        // whitespace, so a wallet may use either one tag with a space-separated
        // list or several tags. Reproduce both.
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
}

impl Drop for NwcClient {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// First `e` tag of an event, as an [`EventId`].
fn first_e_tag(event: &Event) -> Option<EventId> {
    event.tags.iter().find_map(|t| match t.as_slice() {
        [name, value, ..] if name == "e" => EventId::from_hex(value).ok(),
        _ => None,
    })
}

/// Try NIP-04, then NIP-44. Responses are NIP-04 in practice, but a wallet that
/// answers with NIP-44 should still be understood rather than timing out.
fn decrypt_either(keys: &Keys, author: &PublicKey, content: &str) -> Result<String, String> {
    match nip04::decrypt(keys.secret_key(), author, content) {
        Ok(p) => Ok(p),
        Err(first) => nip44::decrypt(keys.secret_key(), author, content)
            .map_err(|second| format!("nip04: {first}; nip44: {second}")),
    }
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::payments::nip47::mock_wallet::{MockWallet, WalletBehavior};
    use crate::payments::nip47::types::{
        NwcError, NwcInvoiceResult, NwcPayInvoiceResult, NOTIFICATION_PAYMENT_RECEIVED,
    };
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

        let client = NwcClient::with_options(
            Arc::clone(&client_pool) as Arc<dyn RelayPoolTrait>,
            connection,
            NwcClientOptions { response_timeout },
        );
        (wallet, client, client_pool)
    }

    #[tokio::test]
    async fn round_trips_make_invoice() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(serde_json::json!({
                    "invoice": "lnbc210n1p",
                    "payment_hash": "hash-1",
                    "state": "pending",
                })),
            )
            .await;

        let env: NwcResponseEnvelope<NwcInvoiceResult> = client
            .request("make_invoice", serde_json::json!({ "amount": 21_000u64 }))
            .await
            .expect("request succeeds");

        assert_eq!(env.result_type.as_deref(), Some("make_invoice"));
        let result = env.into_result().expect("no wallet error");
        assert_eq!(result.invoice.as_deref(), Some("lnbc210n1p"));
        assert_eq!(result.payment_hash.as_deref(), Some("hash-1"));
        assert_eq!(wallet.call_count("make_invoice").await, 1);
    }

    #[tokio::test]
    async fn round_trips_lookup_invoice() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "lookup_invoice",
                WalletBehavior::Answer(serde_json::json!({ "state": "settled" })),
            )
            .await;

        let env: NwcResponseEnvelope<NwcInvoiceResult> = client
            .request("lookup_invoice", serde_json::json!({ "payment_hash": "h" }))
            .await
            .unwrap();
        assert!(env.into_result().unwrap().is_settled());
    }

    #[tokio::test]
    async fn round_trips_pay_invoice() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::json!({ "preimage": "pre", "fees_paid": 12 })),
            )
            .await;

        let env: NwcResponseEnvelope<NwcPayInvoiceResult> = client
            .request("pay_invoice", serde_json::json!({ "invoice": "lnbc1" }))
            .await
            .unwrap();
        let r = env.into_result().unwrap();
        assert_eq!(r.preimage.as_deref(), Some("pre"));
        assert_eq!(r.fees_paid, Some(12));
    }

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

        let env: NwcResponseEnvelope<NwcPayInvoiceResult> = client
            .request("pay_invoice", serde_json::json!({ "invoice": "lnbc1" }))
            .await
            .expect("transport-level call succeeds");

        // The envelope carries the error, and collapsing it is an Err: a failed
        // payment can never be read as a settled one.
        assert_eq!(
            env.error.as_ref().map(NwcError::code_upper).as_deref(),
            Some("INSUFFICIENT_BALANCE")
        );
        assert!(env.into_result().is_err());
    }

    #[tokio::test]
    async fn silence_times_out() {
        let (wallet, client, _pool) = harness_with_timeout(Duration::from_millis(300)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;

        let started = tokio::time::Instant::now();
        let res: Result<NwcResponseEnvelope<NwcInvoiceResult>, _> = client
            .request("make_invoice", serde_json::json!({ "amount": 1000u64 }))
            .await;

        assert!(res.is_err(), "silence must not resolve");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must fail at the configured timeout, not hang"
        );
    }

    #[tokio::test]
    async fn malformed_response_is_an_error() {
        let (wallet, client, _pool) = harness_with_timeout(Duration::from_millis(500)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Malformed)
            .await;

        let res: Result<NwcResponseEnvelope<NwcInvoiceResult>, _> = client
            .request("make_invoice", serde_json::json!({ "amount": 1000u64 }))
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn stranger_signed_response_is_ignored() {
        let (wallet, client, pool) = harness_with_timeout(Duration::from_millis(400)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;
        let client_pubkey = client.client_pubkey();

        // A third party publishes a well-formed response addressed to the
        // client. It must not satisfy the pending request.
        let stranger = Keys::generate();
        let plaintext =
            serde_json::json!({"result_type":"make_invoice","result":{"invoice":"evil"}})
                .to_string();
        let content =
            nip04::encrypt(stranger.secret_key(), &client_pubkey, plaintext).expect("encrypt");
        let evil = EventBuilder::new(Kind::from(23195u16), content)
            .tags([Tag::public_key(client_pubkey)])
            .sign_with_keys(&stranger)
            .expect("sign");
        pool.publish_event(&evil).await.expect("publish");

        let res: Result<NwcResponseEnvelope<NwcInvoiceResult>, _> = client
            .request("make_invoice", serde_json::json!({ "amount": 1000u64 }))
            .await;
        assert!(res.is_err(), "a stranger must not be able to answer");
    }

    #[tokio::test]
    async fn concurrent_requests_correlate_independently() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Delayed {
                    delay: Duration::from_millis(120),
                    result: serde_json::json!({ "invoice": "inv-make" }),
                },
            )
            .await;
        wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::json!({ "preimage": "pre-pay" })),
            )
            .await;

        let client = Arc::new(client);
        let c1 = Arc::clone(&client);
        let c2 = Arc::clone(&client);

        let (make, pay) = tokio::join!(
            async move {
                c1.request::<_, NwcInvoiceResult>(
                    "make_invoice",
                    serde_json::json!({ "amount": 1000u64 }),
                )
                .await
            },
            async move {
                c2.request::<_, NwcPayInvoiceResult>(
                    "pay_invoice",
                    serde_json::json!({ "invoice": "lnbc1" }),
                )
                .await
            }
        );

        // The slow make_invoice must not receive the fast pay_invoice answer.
        assert_eq!(
            make.unwrap().into_result().unwrap().invoice.as_deref(),
            Some("inv-make")
        );
        assert_eq!(
            pay.unwrap().into_result().unwrap().preimage.as_deref(),
            Some("pre-pay")
        );
    }

    #[tokio::test]
    async fn decodes_nip04_notifications() {
        assert_notification_decodes(false).await;
    }

    #[tokio::test]
    async fn decodes_nip44_notifications() {
        assert_notification_decodes(true).await;
    }

    /// Both notification kinds must decode; ts decrypts every kind as NIP-04
    /// and so silently drops the 23197 (NIP-44) form.
    async fn assert_notification_decodes(nip44_mode: bool) {
        let (wallet, client, _pool) = harness().await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        client
            .on_notification(move |payload| {
                let _ = tx.send(payload);
            })
            .await
            .expect("sink registers");

        wallet
            .settle(&client.client_pubkey(), "hash-9", nip44_mode)
            .await
            .expect("wallet publishes notification");

        let payload = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("notification arrives")
            .expect("channel open");

        assert_eq!(payload.notification_type, NOTIFICATION_PAYMENT_RECEIVED);
        assert_eq!(payload.payment_hash(), Some("hash-9"));
    }

    #[tokio::test]
    async fn fetches_info_notification_types() {
        let (wallet, client, _pool) = harness().await;
        wallet
            .publish_info_event(&["payment_received", "payment_sent"])
            .await
            .expect("info published");

        let types = client
            .fetch_info_notification_types()
            .await
            .expect("fetch succeeds");
        assert!(types.iter().any(|t| t == "payment_received"));
        assert!(types.iter().any(|t| t == "payment_sent"));
    }

    #[tokio::test]
    async fn missing_info_event_yields_empty_set_not_an_error() {
        let (_wallet, client, _pool) = harness_with_timeout(Duration::from_millis(400)).await;
        let types = client
            .fetch_info_notification_types()
            .await
            .expect("absence is not a failure");
        assert!(types.is_empty(), "callers fall back to polling on empty");
    }

    /// A response the *wallet itself* signed, but carrying an `e` tag for a
    /// request we never made, must not satisfy a pending waiter. This is the
    /// correlation half of the guard; `stranger_signed_response_is_ignored`
    /// covers the authorship half.
    #[tokio::test]
    async fn uncorrelated_response_is_ignored() {
        let (wallet, client, pool) = harness_with_timeout(Duration::from_millis(400)).await;
        wallet
            .set_behavior("make_invoice", WalletBehavior::Silence)
            .await;
        let client_pubkey = client.client_pubkey();

        // Correctly signed by the wallet, addressed to us, well formed, but
        // correlated to an unrelated event id.
        let unrelated = EventId::all_zeros();
        let plaintext =
            serde_json::json!({"result_type":"make_invoice","result":{"invoice":"wrong"}})
                .to_string();
        let content =
            nip04::encrypt(wallet.keys().secret_key(), &client_pubkey, plaintext).expect("encrypt");
        let event = EventBuilder::new(Kind::from(23195u16), content)
            .tags([Tag::public_key(client_pubkey), Tag::event(unrelated)])
            .sign_with_keys(&wallet.keys())
            .expect("sign");
        pool.publish_event(&event).await.expect("publish");

        let res: Result<NwcResponseEnvelope<NwcInvoiceResult>, _> = client
            .request("make_invoice", serde_json::json!({ "amount": 1000u64 }))
            .await;
        assert!(
            res.is_err(),
            "a response correlated to another request must not settle ours"
        );
    }

    /// The reader task must survive a burst that overruns the broadcast buffer.
    ///
    /// A `Lagged` drops events, but killing the loop there would strand every
    /// waiter forever; continuing means an in-flight request still resolves.
    /// The burst is sized past the pool's 1024-slot channel so the lag path is
    /// exercised, and the assertion is the property that matters either way:
    /// a request issued afterwards still correlates and resolves.
    #[tokio::test]
    async fn reader_survives_broadcast_burst() {
        let (wallet, client, pool) = harness().await;
        wallet
            .set_behavior(
                "make_invoice",
                WalletBehavior::Answer(serde_json::json!({ "invoice": "after-burst" })),
            )
            .await;

        // Flood with unrelated events addressed to nobody in particular.
        let noise = Keys::generate();
        for i in 0..1500u32 {
            let event = EventBuilder::new(Kind::from(1u16), format!("noise-{i}"))
                .sign_with_keys(&noise)
                .expect("sign noise");
            let _ = pool.publish_event(&event).await;
        }

        let env: NwcResponseEnvelope<NwcInvoiceResult> = client
            .request("make_invoice", serde_json::json!({ "amount": 1000u64 }))
            .await
            .expect("reader must still be alive after the burst");
        assert_eq!(
            env.into_result().unwrap().invoice.as_deref(),
            Some("after-burst")
        );
    }
}
