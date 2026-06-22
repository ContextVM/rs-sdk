//! In-memory mock relay pool for network-free testing.
//!
//! Mirrors the design of the TypeScript SDK's `MockRelayHub`:
//! - `publish_event` stores the event and delivers it only to pools whose
//!   `subscribe()` filters match it — like a real relay, a subscriber sees only
//!   the events its subscription selected. A pool that never subscribed (or
//!   subscribed with no matching filter) receives nothing from live publishes.
//! - `subscribe` registers a pool's filters and immediately replays matching
//!   stored events through that pool's channel, so listeners that called
//!   `notifications()` before `subscribe()` see the replay.
//! - `connect` / `disconnect` are no-ops — no sockets are opened.
//! - Signing uses a freshly generated ephemeral `Keys`; `signer()` returns it wrapped in `Arc`
//!   so encryption code can call it without any real relay connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use nostr_sdk::prelude::*;

use crate::core::error::{Error, Result};
use crate::relay::RelayPoolTrait;

/// Process-global source of unique pool ids, so each pool can key its own
/// subscription slot in the shared store without locking at construction time.
static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(0);

// ── Internal state ────────────────────────────────────────────────────────────

/// One linked pool's delivery slot: its broadcast sender plus the filters it
/// registered via `subscribe()`. A live publish is delivered through `tx` only
/// when one of `filters` matches the event.
struct Subscriber {
    tx: tokio::sync::broadcast::Sender<RelayPoolNotification>,
    filters: Vec<Filter>,
}

struct MockRelayInner {
    events: Vec<Event>,
    /// Per-pool subscription slots, keyed by `MockRelayPool::pool_id`. Populated
    /// lazily on a pool's first `subscribe()` call.
    subscribers: HashMap<u64, Subscriber>,
}

impl MockRelayInner {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            subscribers: HashMap::new(),
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
    /// This pool's own broadcast sender. `notifications()` receivers read from
    /// it, and a live publish reaches it only when this pool's registered
    /// `subscribe()` filters match the event.
    notification_tx: tokio::sync::broadcast::Sender<RelayPoolNotification>,
    /// Unique id keying this pool's subscription slot in the shared store.
    pool_id: u64,
    /// Ephemeral key used for signing in `publish` / `sign` / `signer`.
    keys: Keys,
}

impl MockRelayPool {
    /// Create a new mock relay pool with a freshly generated ephemeral signing key.
    pub fn new() -> Self {
        Self::with_keys(Keys::generate())
    }

    /// The ephemeral public key used by this mock for signing.
    pub fn mock_public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// The ephemeral signing keys (for manual event injection in tests).
    pub fn mock_keys(&self) -> Keys {
        self.keys.clone()
    }

    /// Like [`new`](Self::new) but with caller-provided signing keys.
    pub fn with_keys(keys: Keys) -> Self {
        Self::linked(Arc::new(Mutex::new(MockRelayInner::new())), keys)
    }

    /// Build a pool bound to an existing shared store, with its own channel,
    /// pool id, and signing keys.
    fn linked(inner: Arc<Mutex<MockRelayInner>>, keys: Keys) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        Self {
            inner,
            notification_tx: tx,
            pool_id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
            keys,
        }
    }

    /// Create a pair of linked mock relay pools with different signing keys.
    ///
    /// Both pools share the same event store. Each has its own notification
    /// channel; an event published by one reaches the other only when the
    /// other's `subscribe()` filters match it (as a real relay would).
    pub fn create_pair() -> (Self, Self) {
        let inner = Arc::new(Mutex::new(MockRelayInner::new()));
        let a = Self::linked(Arc::clone(&inner), Keys::generate());
        let b = Self::linked(inner, Keys::generate());
        (a, b)
    }

    /// Create `n` linked mock relay pools with different signing keys.
    ///
    /// All pools share the same event store; each gets its own notification
    /// channel and delivery is scoped to each pool's `subscribe()` filters.
    /// Useful for multi-client integration tests.
    pub fn create_linked_group(n: usize) -> Vec<Self> {
        assert!(n > 0, "group must have at least one pool");
        let inner = Arc::new(Mutex::new(MockRelayInner::new()));
        (0..n)
            .map(|_| Self::linked(Arc::clone(&inner), Keys::generate()))
            .collect()
    }

    /// Clone of all events published so far (useful for assertions in tests).
    pub async fn stored_events(&self) -> Vec<Event> {
        self.inner.lock().await.events.clone()
    }

    /// Inject an externally-built event into the store without broadcasting.
    ///
    /// Useful for seeding kind 10002 relay-list events for `fetch_events()` tests.
    pub async fn inject_event(&self, event: Event) {
        self.inner.lock().await.events.push(event);
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

    /// Store the event and deliver it to every linked pool whose registered
    /// `subscribe()` filters match it, mirroring a real relay: a subscriber sees
    /// only the events its subscription selected. Pools with no matching filter
    /// (or no subscription at all) receive nothing.
    async fn publish_event(&self, event: &Event) -> Result<EventId> {
        let event_id = event.id;

        let mut inner = self.inner.lock().await;
        inner.events.push(event.clone());

        for subscriber in inner.subscribers.values() {
            let matches = subscriber
                .filters
                .iter()
                .any(|f| f.match_event(event, MatchEventOptions::default()));
            if matches {
                // Ignore send errors: they just mean this pool has no active receiver.
                let _ = subscriber.tx.send(make_notification(event.clone()));
            }
        }

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

    /// Return a new broadcast receiver for this pool. It only sees events that
    /// match the filters this pool registers via `subscribe()` — events
    /// published *after* this call that match, plus any replayed by a subsequent
    /// `subscribe()`. Without a matching subscription it sees nothing.
    fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
        self.notification_tx.subscribe()
    }

    /// Return the ephemeral public key.
    async fn public_key(&self) -> Result<PublicKey> {
        Ok(self.keys.public_key())
    }

    /// Register this pool's filters (accumulating across calls, like multiple
    /// active REQs) and immediately replay any already-stored events that match
    /// the new filters through this pool's channel, mirroring a real relay that
    /// sends historical events before EOSE.
    async fn subscribe(&self, filters: Vec<Filter>) -> Result<()> {
        let replay = {
            let mut inner = self.inner.lock().await;

            // Snapshot events before touching the subscriber slot to keep the
            // borrows disjoint.
            let events_snapshot = inner.events.clone();

            // Upsert this pool's slot (created on first subscribe), binding it to
            // this pool's own channel and accumulating the new filters.
            let tx = self.notification_tx.clone();
            let subscriber = inner
                .subscribers
                .entry(self.pool_id)
                .or_insert_with(|| Subscriber {
                    tx,
                    filters: Vec::new(),
                });
            subscriber.filters.extend(filters.iter().cloned());

            // Replay only events matching the newly-added filters, as a real
            // relay replays historical events per REQ.
            events_snapshot
                .into_iter()
                .filter(|e| {
                    filters
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

    /// Mock ignores target URLs — delegates to `publish()`.
    async fn publish_to(&self, _urls: &[String], builder: EventBuilder) -> Result<EventId> {
        self.publish(builder).await
    }

    /// Return stored events that match the given filters' kind and author constraints.
    async fn fetch_events(&self, filters: Vec<Filter>, _timeout: Duration) -> Result<Vec<Event>> {
        let inner = self.inner.lock().await;
        let matched: Vec<Event> = inner
            .events
            .iter()
            .filter(|e| {
                filters
                    .iter()
                    .any(|f| f.match_event(e, MatchEventOptions::default()))
            })
            .cloned()
            .collect();
        Ok(matched)
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
    use crate::core::constants::CTXVM_MESSAGES_KIND;

    #[tokio::test]
    async fn connect_and_disconnect_are_noops() {
        let pool = MockRelayPool::new();
        assert!(pool.connect(&["wss://unused".to_string()]).await.is_ok());
        assert!(pool.disconnect().await.is_ok());
    }

    #[tokio::test]
    async fn publish_event_stores_and_delivers_to_matching_subscriber() {
        let pool = MockRelayPool::new();
        let mut rx = pool.notifications();
        // A live publish is only delivered to a pool that subscribed for it.
        pool.subscribe(vec![Filter::new().kind(Kind::TextNote)])
            .await
            .unwrap();

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
    async fn publish_without_subscription_delivers_nothing() {
        let pool = MockRelayPool::new();
        let mut rx = pool.notifications();

        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "unheard")
            .sign_with_keys(&keys)
            .unwrap();
        pool.publish_event(&event).await.unwrap();

        // Stored, but never delivered live without a matching subscription.
        assert_eq!(pool.stored_events().await.len(), 1);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn live_publish_respects_subscriber_filters() {
        let pool = MockRelayPool::new();
        let mut rx = pool.notifications();
        pool.subscribe(vec![Filter::new().kind(Kind::TextNote)])
            .await
            .unwrap();

        let keys = Keys::generate();
        let matching = EventBuilder::new(Kind::TextNote, "keep")
            .sign_with_keys(&keys)
            .unwrap();
        let other = EventBuilder::new(Kind::Custom(9999), "drop")
            .sign_with_keys(&keys)
            .unwrap();
        pool.publish_event(&matching).await.unwrap();
        pool.publish_event(&other).await.unwrap();

        // Only the kind that matches the subscription is delivered.
        let notif = rx.try_recv().unwrap();
        if let RelayPoolNotification::Event { event: e, .. } = notif {
            assert_eq!(e.id, matching.id);
        } else {
            panic!("expected Event notification");
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn subscribe_accumulates_filters_across_calls() {
        // Multiple subscribe() calls behave like multiple active REQs: every
        // registered filter stays live, so events matching any of them deliver.
        let pool = MockRelayPool::new();
        let mut rx = pool.notifications();
        pool.subscribe(vec![Filter::new().kind(Kind::TextNote)])
            .await
            .unwrap();
        pool.subscribe(vec![Filter::new().kind(Kind::Custom(7777))])
            .await
            .unwrap();

        let keys = Keys::generate();
        let kind_a = EventBuilder::new(Kind::TextNote, "a")
            .sign_with_keys(&keys)
            .unwrap();
        let kind_b = EventBuilder::new(Kind::Custom(7777), "b")
            .sign_with_keys(&keys)
            .unwrap();
        let neither = EventBuilder::new(Kind::Custom(9999), "c")
            .sign_with_keys(&keys)
            .unwrap();

        pool.publish_event(&kind_a).await.unwrap();
        pool.publish_event(&kind_b).await.unwrap();
        pool.publish_event(&neither).await.unwrap();

        // Both accumulated filters remain active; the unmatched kind is dropped.
        let first = rx.try_recv().unwrap();
        let second = rx.try_recv().unwrap();
        let received: Vec<EventId> = [first, second]
            .into_iter()
            .map(|n| match n {
                RelayPoolNotification::Event { event, .. } => event.id,
                _ => panic!("expected Event notification"),
            })
            .collect();
        assert!(received.contains(&kind_a.id), "kind A must be delivered");
        assert!(received.contains(&kind_b.id), "kind B must be delivered");
        assert!(
            !received.contains(&neither.id),
            "unmatched kind must not be delivered"
        );
        assert!(
            rx.try_recv().is_err(),
            "only the two matching events deliver"
        );
    }

    #[tokio::test]
    async fn linked_pools_only_receive_their_subscribed_events() {
        // The fix that makes EncryptionMode::Disabled e2e tests work: a pool
        // subscribed to its own p-tag never receives an event addressed to a peer.
        let (a, b) = MockRelayPool::create_pair();
        let a_pubkey = a.mock_public_key();
        let b_pubkey = b.mock_public_key();
        let mut a_rx = a.notifications();
        let mut b_rx = b.notifications();

        // Each pool subscribes for events p-tagged to itself.
        a.subscribe(vec![Filter::new()
            .kind(Kind::Custom(CTXVM_MESSAGES_KIND))
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                a_pubkey.to_hex(),
            )])
        .await
        .unwrap();
        b.subscribe(vec![Filter::new()
            .kind(Kind::Custom(CTXVM_MESSAGES_KIND))
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                b_pubkey.to_hex(),
            )])
        .await
        .unwrap();

        // `a` publishes a response addressed to `b` (p-tag = b).
        let keys = Keys::generate();
        let response = EventBuilder::new(Kind::Custom(CTXVM_MESSAGES_KIND), "to-b")
            .tag(Tag::public_key(b_pubkey))
            .sign_with_keys(&keys)
            .unwrap();
        a.publish_event(&response).await.unwrap();

        // `b` receives it; `a` does NOT echo its own peer-addressed event.
        let notif = b_rx.try_recv().unwrap();
        if let RelayPoolNotification::Event { event: e, .. } = notif {
            assert_eq!(e.id, response.id);
        } else {
            panic!("expected Event notification for b");
        }
        assert!(
            a_rx.try_recv().is_err(),
            "a must not receive its own b-addressed event"
        );
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

        // No subscription yet, so nothing was delivered live.
        assert!(rx.try_recv().is_err());

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
        pool.subscribe(vec![Filter::new().kind(Kind::TextNote)])
            .await
            .unwrap();

        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "future")
            .sign_with_keys(&keys)
            .unwrap();
        pool.publish_event(&event).await.unwrap();

        let notif = rx.try_recv().unwrap();
        assert!(matches!(notif, RelayPoolNotification::Event { .. }));
    }
}
