//! Scripted NIP-47 wallet service for tests, behind `test-utils`.
//!
//! Runs the *real* wire protocol over a linked [`MockRelayPool`]: it subscribes
//! for kind-23194 requests addressed to it, decrypts them, and publishes
//! kind-23195 responses (and notifications). That means
//! [`NwcClient`](super::NwcClient) exercises its own encrypt, publish,
//! correlate and decrypt paths in CI rather than being stubbed out.
//!
//! # It must be able to misbehave
//!
//! A mock written alongside the client agrees with the client wherever the
//! client is wrong, so every knob here exists to break one assumption the
//! client would otherwise never have tested: omitting the `p` tag, answering
//! out of order, answering with a wrong-typed field, mislabelling
//! `result_type`, dropping the `e` tag, encrypting a notification the other
//! way, or publishing one notification under both kinds.
//!
//! Requests are served on a spawned task each, so a fast answer really can
//! overtake a slow one. Serving them inline would make the wallet strictly
//! FIFO and no correlation test could ever fail.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::relay::{MockRelayPool, RelayPoolTrait};

const KIND_REQUEST: u16 = 23194;
const KIND_RESPONSE: u16 = 23195;
const KIND_NOTIFICATION_LEGACY: u16 = 23196;
const KIND_NOTIFICATION: u16 = 23197;
const KIND_INFO: u16 = 13194;

/// How the mock wallet answers one method.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WalletBehavior {
    /// Reply with this `result` object.
    Answer(serde_json::Value),
    /// Reply with a NIP-47 error envelope.
    Error {
        /// Error code, e.g. `NOT_FOUND`.
        code: String,
        /// Human-readable message. May contain anything a real wallet might.
        message: String,
    },
    /// Never reply, so the caller hits its own deadline.
    Silence,
    /// Reply with `result` after a delay. Served on its own task, so a later
    /// request with a shorter delay answers first.
    Delayed {
        /// Delay before replying.
        delay: Duration,
        /// The `result` object to send.
        result: serde_json::Value,
    },
    /// Reply with content that is not valid JSON once decrypted.
    Malformed,
    /// Reply with a `result_type` that does not match the request method.
    MislabelledResultType {
        /// The wrong label to send.
        result_type: String,
        /// The `result` object to send.
        result: serde_json::Value,
    },
    /// Reply without the `e` tag, so nothing can correlate it.
    NoCorrelationTag(serde_json::Value),
}

/// Knobs that break an assumption the client might otherwise be making.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MockWalletQuirks {
    /// Omit the `p` tag on responses and notifications. A wallet that does
    /// this is invisible to a `#p`-filtered subscription.
    pub omit_p_tag: bool,
    /// Answer an unscripted method with `NOT_IMPLEMENTED` instead of silence,
    /// which is what a real wallet does for a method it lacks.
    pub not_implemented_for_unscripted: bool,
    /// Publish each notification under BOTH notification kinds, as a wallet
    /// supporting both encryptions does.
    pub duplicate_notification_kinds: bool,
    /// Encrypt notifications with NIP-44 rather than NIP-04.
    pub nip44_notifications: bool,
}

/// A scripted NIP-47 wallet service on a mock relay network.
pub struct MockWallet {
    pool: Arc<MockRelayPool>,
    keys: Keys,
    behaviors: Arc<Mutex<HashMap<String, WalletBehavior>>>,
    quirks: Arc<Mutex<MockWalletQuirks>>,
    calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    cancel: CancellationToken,
}

impl MockWallet {
    /// Build a wallet on `pool` with freshly generated keys.
    pub fn new(pool: Arc<MockRelayPool>) -> Self {
        Self::with_keys(pool, Keys::generate())
    }

    /// Build a wallet on `pool` under caller-chosen keys.
    pub fn with_keys(pool: Arc<MockRelayPool>, keys: Keys) -> Self {
        Self {
            pool,
            keys,
            behaviors: Arc::new(Mutex::new(HashMap::new())),
            quirks: Arc::new(Mutex::new(MockWalletQuirks::default())),
            calls: Arc::new(Mutex::new(Vec::new())),
            cancel: CancellationToken::new(),
        }
    }

    /// The wallet service public key, for a connection string.
    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// The wallet's signing keys, so a test can forge an event that is
    /// genuinely wallet-signed.
    pub fn keys(&self) -> Keys {
        self.keys.clone()
    }

    /// Script how `method` is answered. Later calls replace earlier ones.
    pub async fn set_behavior(&self, method: &str, behavior: WalletBehavior) {
        self.behaviors
            .lock()
            .await
            .insert(method.to_string(), behavior);
    }

    /// Replace the quirk set.
    pub async fn set_quirks(&self, quirks: MockWalletQuirks) {
        *self.quirks.lock().await = quirks;
    }

    /// Methods received so far, with their params, in arrival order.
    pub async fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.calls.lock().await.clone()
    }

    /// How many times `method` was called.
    pub async fn call_count(&self, method: &str) -> usize {
        self.calls
            .lock()
            .await
            .iter()
            .filter(|(m, _)| m == method)
            .count()
    }

    /// Publish a kind-13194 info event advertising `notification_types`.
    pub async fn publish_info_event(&self, notification_types: &[&str]) -> Result<(), String> {
        let builder = EventBuilder::new(
            Kind::from(KIND_INFO),
            "pay_invoice make_invoice lookup_invoice",
        )
        .tags(
            [Tag::parse(["notifications", &notification_types.join(" ")])
                .map_err(|e| e.to_string())?],
        );
        let event = builder
            .sign_with_keys(&self.keys)
            .map_err(|e| e.to_string())?;
        self.pool
            .publish_event(&event)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Publish a `payment_received` notification to `client_pubkey`.
    ///
    /// Encryption and kind follow [`MockWalletQuirks`]; with
    /// `duplicate_notification_kinds` the same payload goes out under both
    /// kinds, which is what a dual-encryption wallet really does.
    pub async fn settle(
        &self,
        client_pubkey: &PublicKey,
        payment_hash: &str,
    ) -> Result<(), String> {
        let quirks = self.quirks.lock().await.clone();
        let payload = serde_json::json!({
            "notification_type": "payment_received",
            "notification": {
                "payment_hash": payment_hash,
                "state": "settled",
                "settled_at": 1_700_000_000i64,
            }
        })
        .to_string();

        let kinds: &[u16] = if quirks.duplicate_notification_kinds {
            &[KIND_NOTIFICATION_LEGACY, KIND_NOTIFICATION]
        } else if quirks.nip44_notifications {
            &[KIND_NOTIFICATION]
        } else {
            &[KIND_NOTIFICATION_LEGACY]
        };

        for kind in kinds {
            let content = if quirks.nip44_notifications && !quirks.duplicate_notification_kinds {
                nip44::encrypt(
                    self.keys.secret_key(),
                    client_pubkey,
                    &payload,
                    nip44::Version::default(),
                )
                .map_err(|e| e.to_string())?
            } else {
                nip04::encrypt(self.keys.secret_key(), client_pubkey, &payload)
                    .map_err(|e| e.to_string())?
            };

            let mut builder = EventBuilder::new(Kind::from(*kind), content);
            if !quirks.omit_p_tag {
                builder = builder.tags([Tag::public_key(*client_pubkey)]);
            }
            let event = builder
                .sign_with_keys(&self.keys)
                .map_err(|e| e.to_string())?;
            self.pool
                .publish_event(&event)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Publish a NIP-04-encrypted notification on kind 23197, which is ts's own
    /// fixture and which a decrypt-by-kind client drops.
    pub async fn settle_nip04_on_nip44_kind(
        &self,
        client_pubkey: &PublicKey,
        payment_hash: &str,
    ) -> Result<(), String> {
        let payload = serde_json::json!({
            "notification_type": "payment_received",
            "notification": { "payment_hash": payment_hash }
        })
        .to_string();
        let content = nip04::encrypt(self.keys.secret_key(), client_pubkey, &payload)
            .map_err(|e| e.to_string())?;
        let event = EventBuilder::new(Kind::from(KIND_NOTIFICATION), content)
            .tags([Tag::public_key(*client_pubkey)])
            .sign_with_keys(&self.keys)
            .map_err(|e| e.to_string())?;
        self.pool
            .publish_event(&event)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Subscribe for requests and start answering. Call once.
    pub async fn start(&self) -> Result<(), String> {
        let filter = Filter::new()
            .kind(Kind::from(KIND_REQUEST))
            .pubkey(self.keys.public_key());

        let notifications = self.pool.notifications();
        self.pool
            .subscribe(vec![filter])
            .await
            .map_err(|e| e.to_string())?;

        let pool = Arc::clone(&self.pool);
        let keys = self.keys.clone();
        let behaviors = Arc::clone(&self.behaviors);
        let quirks = Arc::clone(&self.quirks);
        let calls = Arc::clone(&self.calls);
        let cancel = self.cancel.clone();

        tokio::spawn(async move {
            Self::serve(notifications, pool, keys, behaviors, quirks, calls, cancel).await;
        });
        Ok(())
    }

    /// Stop serving. Idempotent.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    async fn serve(
        mut notifications: tokio::sync::broadcast::Receiver<RelayPoolNotification>,
        pool: Arc<MockRelayPool>,
        keys: Keys,
        behaviors: Arc<Mutex<HashMap<String, WalletBehavior>>>,
        quirks: Arc<Mutex<MockWalletQuirks>>,
        calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        cancel: CancellationToken,
    ) {
        loop {
            let notification = tokio::select! {
                _ = cancel.cancelled() => break,
                result = notifications.recv() => match result {
                    Ok(n) => n,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            };

            let RelayPoolNotification::Event { event, .. } = notification else {
                continue;
            };
            if event.kind.as_u16() != KIND_REQUEST {
                continue;
            }

            let Ok(plaintext) = nip04::decrypt(keys.secret_key(), &event.pubkey, &event.content)
            else {
                continue;
            };
            let Ok(body) = serde_json::from_str::<serde_json::Value>(&plaintext) else {
                continue;
            };
            let method = body
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            let params = body
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            calls.lock().await.push((method.clone(), params));

            let quirks_now = quirks.lock().await.clone();
            let behavior = behaviors.lock().await.get(&method).cloned();
            let behavior = match behavior {
                Some(b) => b,
                None if quirks_now.not_implemented_for_unscripted => WalletBehavior::Error {
                    code: "NOT_IMPLEMENTED".to_string(),
                    message: format!("{method} is not supported by this wallet"),
                },
                // Default: silence, like a wallet that never answers.
                None => continue,
            };

            // One task per request, so a fast answer can overtake a slow one.
            // Serving inline would make this wallet strictly FIFO and no
            // correlation test could ever fail.
            let pool = Arc::clone(&pool);
            let keys = keys.clone();
            tokio::spawn(async move {
                Self::answer(pool, keys, *event, method, behavior, quirks_now).await;
            });
        }
    }

    async fn answer(
        pool: Arc<MockRelayPool>,
        keys: Keys,
        request: Event,
        method: String,
        behavior: WalletBehavior,
        quirks: MockWalletQuirks,
    ) {
        let mut result_type = method.clone();
        let mut correlate = true;

        let (result, error, malformed) = match behavior {
            WalletBehavior::Silence => return,
            WalletBehavior::Answer(result) => (Some(result), None, false),
            WalletBehavior::Delayed { delay, result } => {
                tokio::time::sleep(delay).await;
                (Some(result), None, false)
            }
            WalletBehavior::Error { code, message } => (
                None,
                Some(serde_json::json!({ "code": code, "message": message })),
                false,
            ),
            WalletBehavior::Malformed => (None, None, true),
            WalletBehavior::MislabelledResultType {
                result_type: label,
                result,
            } => {
                result_type = label;
                (Some(result), None, false)
            }
            WalletBehavior::NoCorrelationTag(result) => {
                correlate = false;
                (Some(result), None, false)
            }
        };

        let plaintext = if malformed {
            "{ this is not json".to_string()
        } else {
            serde_json::json!({
                "result_type": result_type,
                "error": error,
                "result": result,
            })
            .to_string()
        };

        let Ok(content) = nip04::encrypt(keys.secret_key(), &request.pubkey, plaintext) else {
            return;
        };

        let mut tags = Vec::new();
        if !quirks.omit_p_tag {
            tags.push(Tag::public_key(request.pubkey));
        }
        if correlate {
            tags.push(Tag::event(request.id));
        }

        let Ok(response) = EventBuilder::new(Kind::from(KIND_RESPONSE), content)
            .tags(tags)
            .sign_with_keys(&keys)
        else {
            return;
        };
        let _ = pool.publish_event(&response).await;
    }
}

impl Drop for MockWallet {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
