//! In-memory mock relay pool for network-free testing.
//!
//! Mirrors the design of the TypeScript SDK's `MockRelayHub`:
//! - `publish_event` stores the event and broadcasts it to all `notifications()` receivers.
//! - `subscribe` registers filters and immediately replays matching stored events through the
//!   broadcast, so listeners that called `notifications()` before `subscribe()` see the replay.
//! - `connect` / `disconnect` are no-ops — no sockets are opened.
//! - Signing uses a freshly generated ephemeral `Keys`; `signer()` returns it wrapped in `Arc`
//!   so encryption code can call it without any real relay connection.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use nostr_sdk::prelude::*;

use crate::core::error::{Error, Result};
use crate::relay::RelayPoolTrait;

// ── Internal state ────────────────────────────────────────────────────────────

struct MockRelayInner {
    events: Vec<Event>,
    /// Active subscriptions: id → filters registered by that subscription.
    subscriptions: HashMap<u32, Vec<Filter>>,
    next_sub_id: u32,
}

impl MockRelayInner {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            subscriptions: HashMap::new(),
            next_sub_id: 0,
        }
    }
}

// ── Public struct ─────────────────────────────────────────────────────────────

/// In-memory relay pool for deterministic, network-free testing.
///
/// Create one with [`MockRelayPool::new`] and pass it (wrapped in `Arc`) wherever
/// an `Arc<dyn RelayPoolTrait>` is expected.
pub struct MockRelayPool {
    inner: Arc<Mutex<MockRelayInner>>,
    /// Broadcast sender — every published event is sent here so that all
    /// `notifications()` receivers see it.
    notification_tx: tokio::sync::broadcast::Sender<RelayPoolNotification>,
    /// Ephemeral key used for signing in `publish` / `sign` / `signer`.
    keys: Keys,
}

impl MockRelayPool {
    /// Create a new mock relay pool with a freshly generated ephemeral signing key.
    pub fn new() -> Self {
        let keys = Keys::generate();
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        Self {
            inner: Arc::new(Mutex::new(MockRelayInner::new())),
            notification_tx: tx,
            keys,
        }
    }

    /// The ephemeral public key used by this mock for signing.
    pub fn mock_public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// Like [`new`](Self::new) but with caller-provided signing keys.
    pub fn with_keys(keys: Keys) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        Self {
            inner: Arc::new(Mutex::new(MockRelayInner::new())),
            notification_tx: tx,
            keys,
        }
    }

    /// Create a pair of linked mock relay pools with different signing keys.
    ///
    /// Both pools share the same event store and notification channel; events
    /// published by one are visible to the other's `notifications()` receivers.
    pub fn create_pair() -> (Self, Self) {
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        let inner = Arc::new(Mutex::new(MockRelayInner::new()));
        let a = Self {
            inner: Arc::clone(&inner),
            notification_tx: tx.clone(),
            keys: Keys::generate(),
        };
        let b = Self {
            inner,
            notification_tx: tx,
            keys: Keys::generate(),
        };
        (a, b)
    }

    /// Clone of all events published so far (useful for assertions in tests).
    pub async fn stored_events(&self) -> Vec<Event> {
        self.inner.lock().await.events.clone()
    }
}

impl Default for MockRelayPool {
    fn default() -> Self {
        Self::new()
    }
}

// ── RelayPoolTrait impl ───────────────────────────────────────────────────────

#[async_trait]
impl RelayPoolTrait for MockRelayPool {
    /// No-op: the mock has no sockets to open.
    async fn connect(&self, _relay_urls: &[String]) -> Result<()> {
        Ok(())
    }

    /// No-op: the mock has no sockets to close.
    async fn disconnect(&self) -> Result<()> {
        Ok(())
    }

    /// Store the event and broadcast it to all current `notifications()` receivers.
    async fn publish_event(&self, event: &Event) -> Result<EventId> {
        let event_id = event.id;

        {
            let mut inner = self.inner.lock().await;
            inner.events.push(event.clone());
        }

        // Always broadcast — consumers filter by kind/pubkey/tag themselves,
        // which mirrors how nostr-sdk's real notification stream works.
        let notification = make_notification(event.clone());
        // Ignore send errors: they just mean there are no active receivers yet.
        let _ = self.notification_tx.send(notification);

        Ok(event_id)
    }

    /// Sign `builder` with the ephemeral key, then call `publish_event`.
    async fn publish(&self, builder: EventBuilder) -> Result<EventId> {
        let event = sign_with_keys(builder, &self.keys)?;
        let id = event.id;
        self.publish_event(&event).await?;
        Ok(id)
    }

    /// Sign `builder` with the ephemeral key and return the event without publishing.
    async fn sign(&self, builder: EventBuilder) -> Result<Event> {
        sign_with_keys(builder, &self.keys)
    }

    /// Return the ephemeral key as a signer.
    async fn signer(&self) -> Result<Arc<dyn NostrSigner>> {
        Ok(Arc::new(self.keys.clone()) as Arc<dyn NostrSigner>)
    }

    /// Return a new broadcast receiver. Each call gets an independent receiver
    /// that sees all events published *after* this call, plus any replayed by
    /// a subsequent `subscribe()`.
    fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
        self.notification_tx.subscribe()
    }

    /// Return the ephemeral public key.
    async fn public_key(&self) -> Result<PublicKey> {
        Ok(self.keys.public_key())
    }

    /// Register the filters and immediately replay any already-stored events that
    /// match them through the broadcast channel, mirroring the behaviour of a
    /// real relay that sends historical events before EOSE.
    async fn subscribe(&self, filters: Vec<Filter>) -> Result<()> {
        let replay = {
            let mut inner = self.inner.lock().await;
            let sub_id = inner.next_sub_id;
            inner.next_sub_id += 1;

            // Store filters first so the replay read comes from the stored value,
            // ensuring the field is both written and read (no dead-code warning).
            inner.subscriptions.insert(sub_id, filters);

            // Clone events so we can release the events borrow before borrowing subscriptions.
            let events_snapshot = inner.events.clone();
            let stored = inner.subscriptions.get(&sub_id).expect("just inserted");
            events_snapshot
                .into_iter()
                .filter(|e| {
                    stored
                        .iter()
                        .any(|f| f.match_event(e, MatchEventOptions::default()))
                })
                .collect::<Vec<_>>()
        };

        for event in replay {
            let _ = self.notification_tx.send(make_notification(event));
        }

        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn sign_with_keys(builder: EventBuilder, keys: &Keys) -> Result<Event> {
    builder
        .sign_with_keys(keys)
        .map_err(|e| Error::Transport(e.to_string()))
}

fn make_notification(event: Event) -> RelayPoolNotification {
    RelayPoolNotification::Event {
        relay_url: RelayUrl::parse("wss://mock.relay").expect("hardcoded URL"),
        subscription_id: SubscriptionId::generate(),
        event: Box::new(event),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_and_disconnect_are_noops() {
        let pool = MockRelayPool::new();
        assert!(pool.connect(&["wss://unused".to_string()]).await.is_ok());
        assert!(pool.disconnect().await.is_ok());
    }

    #[tokio::test]
    async fn publish_event_stores_and_broadcasts() {
        let pool = MockRelayPool::new();
        let mut rx = pool.notifications();

        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "hello")
            .sign_with_keys(&keys)
            .unwrap();

        pool.publish_event(&event).await.unwrap();

        assert_eq!(pool.stored_events().await.len(), 1);
        let notif = rx.try_recv().unwrap();
        if let RelayPoolNotification::Event { event: e, .. } = notif {
            assert_eq!(e.id, event.id);
        } else {
            panic!("expected Event notification");
        }
    }

    #[tokio::test]
    async fn publish_signs_and_stores() {
        let pool = MockRelayPool::new();
        let builder = EventBuilder::new(Kind::TextNote, "signed");
        pool.publish(builder).await.unwrap();
        let stored = pool.stored_events().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].pubkey, pool.mock_public_key());
    }

    #[tokio::test]
    async fn sign_does_not_publish() {
        let pool = MockRelayPool::new();
        let builder = EventBuilder::new(Kind::TextNote, "unsigned");
        let event = pool.sign(builder).await.unwrap();
        assert_eq!(event.pubkey, pool.mock_public_key());
        assert!(pool.stored_events().await.is_empty());
    }

    #[tokio::test]
    async fn signer_uses_same_key_as_publish() {
        let pool = MockRelayPool::new();
        let signer = pool.signer().await.unwrap();
        let expected_pubkey = pool.mock_public_key();
        assert_eq!(signer.get_public_key().await.unwrap(), expected_pubkey);
    }

    #[tokio::test]
    async fn subscribe_replays_matching_stored_events() {
        let pool = MockRelayPool::new();
        let mut rx = pool.notifications();

        // Pre-publish two events
        let keys = Keys::generate();
        let e1 = EventBuilder::new(Kind::TextNote, "one")
            .sign_with_keys(&keys)
            .unwrap();
        let e2 = EventBuilder::new(Kind::Custom(9999), "two")
            .sign_with_keys(&keys)
            .unwrap();
        pool.publish_event(&e1).await.unwrap();
        pool.publish_event(&e2).await.unwrap();

        // Drain the two publish notifications
        rx.try_recv().unwrap();
        rx.try_recv().unwrap();

        // Subscribe for TextNote only — e1 should be replayed, e2 not
        let filter = Filter::new().kind(Kind::TextNote);
        pool.subscribe(vec![filter]).await.unwrap();

        let replayed = rx.try_recv().unwrap();
        if let RelayPoolNotification::Event { event, .. } = replayed {
            assert_eq!(event.id, e1.id);
        } else {
            panic!("expected replayed Event notification");
        }
        // e2 should not be replayed
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn notifications_receives_future_publishes() {
        let pool = MockRelayPool::new();
        let mut rx = pool.notifications();

        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "future")
            .sign_with_keys(&keys)
            .unwrap();
        pool.publish_event(&event).await.unwrap();

        let notif = rx.try_recv().unwrap();
        assert!(matches!(notif, RelayPoolNotification::Event { .. }));
    }
}
