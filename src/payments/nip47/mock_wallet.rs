//! Scripted NIP-47 wallet service for tests, behind `test-utils`.
//!
//! Runs the *real* wire protocol over a linked [`MockRelayPool`]: it subscribes
//! for kind-23194 requests addressed to it, decrypts them, and publishes
//! kind-23195 responses (and kind-23196/23197 notifications). That means
//! [`NwcClient`](crate::payments::nip47::NwcClient) exercises its own encrypt,
//! publish, correlate, and decrypt paths in CI, rather than being stubbed out.
//!
//! Behaviors are scripted per method: answer, error, silence, delay, malformed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::relay::{MockRelayPool, RelayPoolTrait};

const KIND_REQUEST: u16 = 23194;
const KIND_RESPONSE: u16 = 23195;
const KIND_NOTIFICATION_NIP04: u16 = 23196;
const KIND_NOTIFICATION_NIP44: u16 = 23197;
const KIND_INFO: u16 = 13194;

/// How the mock wallet answers one method.
#[derive(Debug, Clone)]
pub enum WalletBehavior {
    /// Reply with this `result` object.
    Answer(serde_json::Value),
    /// Reply with a NIP-47 error envelope.
    Error {
        /// Error code, e.g. `NOT_FOUND`.
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// Never reply, so the caller hits its own timeout.
    Silence,
    /// Reply with `result` after a delay.
    Delayed {
        /// Delay before replying.
        delay: Duration,
        /// The `result` object to send.
        result: serde_json::Value,
    },
    /// Reply with content that is not valid JSON once decrypted.
    Malformed,
}

/// A scripted NIP-47 wallet service on a mock relay network.
pub struct MockWallet {
    pool: Arc<MockRelayPool>,
    keys: Keys,
    behaviors: Arc<Mutex<HashMap<String, WalletBehavior>>>,
    /// Methods seen, in arrival order, for assertions.
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
            calls: Arc::new(Mutex::new(Vec::new())),
            cancel: CancellationToken::new(),
        }
    }

    /// The wallet service public key, for a connection string.
    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// The wallet's signing keys, so a test can forge an event that is
    /// genuinely wallet-signed (for example a correctly authored but
    /// mis-correlated response).
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
    /// `nip44` selects kind 23197 (NIP-44) over the legacy kind 23196 (NIP-04),
    /// so both decode paths are exercisable.
    pub async fn settle(
        &self,
        client_pubkey: &PublicKey,
        payment_hash: &str,
        nip44_mode: bool,
    ) -> Result<(), String> {
        let payload = serde_json::json!({
            "notification_type": "payment_received",
            "notification": {
                "payment_hash": payment_hash,
                "state": "settled",
                "settled_at": 1_700_000_000i64,
            }
        });
        let plaintext = payload.to_string();

        let (kind, content) = if nip44_mode {
            (
                KIND_NOTIFICATION_NIP44,
                nip44::encrypt(
                    self.keys.secret_key(),
                    client_pubkey,
                    plaintext,
                    nip44::Version::default(),
                )
                .map_err(|e| e.to_string())?,
            )
        } else {
            (
                KIND_NOTIFICATION_NIP04,
                nip04::encrypt(self.keys.secret_key(), client_pubkey, plaintext)
                    .map_err(|e| e.to_string())?,
            )
        };

        let event = EventBuilder::new(Kind::from(kind), content)
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
        let calls = Arc::clone(&self.calls);
        let cancel = self.cancel.clone();

        tokio::spawn(async move {
            Self::serve(notifications, pool, keys, behaviors, calls, cancel).await;
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

            let behavior = behaviors.lock().await.get(&method).cloned();
            let Some(behavior) = behavior else {
                // Unscripted method: stay silent, like a wallet that does not
                // implement it and never answers.
                continue;
            };

            let (result, error, malformed) = match behavior {
                WalletBehavior::Silence => continue,
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
            };

            let plaintext = if malformed {
                "{ this is not json".to_string()
            } else {
                serde_json::json!({
                    "result_type": method,
                    "error": error,
                    "result": result,
                })
                .to_string()
            };

            let Ok(content) = nip04::encrypt(keys.secret_key(), &event.pubkey, plaintext) else {
                continue;
            };

            let builder = EventBuilder::new(Kind::from(KIND_RESPONSE), content)
                .tags([Tag::public_key(event.pubkey), Tag::event(event.id)]);
            let Ok(response) = builder.sign_with_keys(&keys) else {
                continue;
            };
            let _ = pool.publish_event(&response).await;
        }
    }
}

impl Drop for MockWallet {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
