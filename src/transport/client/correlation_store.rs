//! Client-side correlation store for tracking pending request event IDs.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use tokio::sync::RwLock;

use crate::core::constants::DEFAULT_LRU_SIZE;

/// A pending request tracked by the correlation store.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    /// The original JSON-RPC request ID before event-ID replacement.
    pub original_id: serde_json::Value,
    /// Whether this request is an `initialize` handshake.
    pub is_initialize: bool,
    /// When the request was registered.
    pub registered_at: Instant,
    /// CEP-8: the original `_meta.progressToken` JSON value the request carried,
    /// exactly as sent. Client-side payment work fabricates keep-alive progress
    /// with this value, never a stringified form: rmcp's progress watcher is
    /// keyed by the token's exact JSON type.
    pub progress_token: Option<serde_json::Value>,
}

/// Tracks pending request event IDs and their original request IDs on the client side.
///
/// An optional capacity limit enables LRU eviction of the oldest entry when the
/// store is full.
#[derive(Clone)]
pub struct ClientCorrelationStore {
    pending_requests: Arc<RwLock<LruCache<String, PendingRequest>>>,
}

impl Default for ClientCorrelationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientCorrelationStore {
    /// Create a new store with the default capacity
    pub fn new() -> Self {
        Self::with_max_pending(DEFAULT_LRU_SIZE)
    }

    /// Create a store with an upper bound on pending requests.
    /// When the limit is reached the oldest entry is evicted.
    pub fn with_max_pending(max_pending: usize) -> Self {
        Self {
            pending_requests: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(max_pending).unwrap_or(NonZeroUsize::new(1).unwrap()),
            ))),
        }
    }

    /// Register a pending request with its original JSON-RPC request ID and, when
    /// the request carried one, its original `_meta.progressToken` JSON value.
    pub async fn register(
        &self,
        event_id: String,
        original_id: serde_json::Value,
        is_initialize: bool,
        progress_token: Option<serde_json::Value>,
    ) {
        self.pending_requests.write().await.push(
            event_id,
            PendingRequest {
                original_id,
                is_initialize,
                registered_at: Instant::now(),
                progress_token,
            },
        );
    }

    /// Check whether a given event ID corresponds to an `initialize` request.
    pub async fn is_initialize_request(&self, event_id: &str) -> bool {
        self.pending_requests
            .read()
            .await
            .peek(event_id)
            .is_some_and(|r| r.is_initialize)
    }

    /// Check whether a pending request exists for the given event ID
    pub async fn contains(&self, event_id: &str) -> bool {
        self.pending_requests.read().await.contains(event_id)
    }

    /// Remove a pending request, returning the consumed entry if the key existed.
    /// The caller gets the entry's `original_id` and `progress_token`, which the
    /// client-side payment hooks need at consumption time.
    pub async fn remove(&self, event_id: &str) -> Option<PendingRequest> {
        self.pending_requests.write().await.pop(event_id)
    }

    /// A clone of the pending entry for `event_id`, without removing it or
    /// promoting its LRU recency. The client-side payment hooks read the
    /// entry's `original_id` and `progress_token` while the request stays
    /// pending.
    pub async fn peek(&self, event_id: &str) -> Option<PendingRequest> {
        self.pending_requests.read().await.peek(event_id).cloned()
    }

    /// Retrieve the original request ID for a given event ID without removing it.
    pub async fn get_original_id(&self, event_id: &str) -> Option<serde_json::Value> {
        self.pending_requests
            .read()
            .await
            .peek(event_id)
            .map(|r| r.original_id.clone())
    }

    /// Refresh a pending request's registration timestamp without disturbing its
    /// `original_id`/`is_initialize`. Used by CEP-22 oversized transfers: each
    /// inbound frame "touches" the entry so [`sweep_expired`](Self::sweep_expired)
    /// does not evict it mid-transfer. Returns `true` if the entry existed.
    pub async fn touch(&self, event_id: &str) -> bool {
        let mut cache = self.pending_requests.write().await;
        if let Some(entry) = cache.get_mut(event_id) {
            entry.registered_at = Instant::now();
            true
        } else {
            false
        }
    }

    /// Number of pending requests currently tracked.
    pub async fn count(&self) -> usize {
        self.pending_requests.read().await.len()
    }

    /// Remove all entries older than `timeout`. Returns the number of entries removed.
    pub async fn sweep_expired(&self, timeout: Duration) -> usize {
        let now = Instant::now();
        let mut cache = self.pending_requests.write().await;
        let mut expired_keys = Vec::new();

        for (key, entry) in cache.iter() {
            if now.duration_since(entry.registered_at) >= timeout {
                expired_keys.push(key.clone());
            }
        }

        let count = expired_keys.len();
        for key in expired_keys {
            cache.pop(&key);
        }
        count
    }

    /// Remove all pending requests from the store
    pub async fn clear(&self) {
        self.pending_requests.write().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remove_nonexistent_is_noop() {
        let store = ClientCorrelationStore::new();
        assert!(store.remove("nonexistent").await.is_none());
        assert!(!store.contains("nonexistent").await);
    }

    #[tokio::test]
    async fn contains_after_clear() {
        let store = ClientCorrelationStore::new();
        store
            .register("e1".into(), serde_json::Value::Null, false, None)
            .await;
        store
            .register("e2".into(), serde_json::Value::Null, false, None)
            .await;
        assert!(store.contains("e1").await);
        store.clear().await;
        assert!(!store.contains("e1").await);
        assert!(!store.contains("e2").await);
    }

    #[tokio::test]
    async fn register_and_remove_roundtrip() {
        let store = ClientCorrelationStore::new();
        store
            .register("e1".into(), serde_json::Value::Null, false, None)
            .await;
        assert!(store.contains("e1").await);
        assert!(store.remove("e1").await.is_some());
        assert!(!store.contains("e1").await);
    }

    #[tokio::test]
    async fn default_store_is_bounded() {
        let store = ClientCorrelationStore::new();
        for i in 0..=DEFAULT_LRU_SIZE {
            store
                .register(format!("e{i}"), serde_json::Value::Null, false, None)
                .await;
        }

        assert_eq!(store.count().await, DEFAULT_LRU_SIZE);
        assert!(!store.contains("e0").await);
        assert!(store.contains(&format!("e{DEFAULT_LRU_SIZE}")).await);
    }

    #[tokio::test]
    async fn sweep_expired_removes_only_stale_entries() {
        let store = ClientCorrelationStore::new();

        // Insert an entry that will be "old" by the time we sweep.
        store
            .register("old".into(), serde_json::json!(1), false, None)
            .await;

        // Sleep so "old" entry ages past the threshold.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Insert a fresh entry.
        store
            .register("fresh".into(), serde_json::json!(2), false, None)
            .await;

        // Sweep with a 10ms timeout — "old" should be removed, "fresh" should remain.
        let swept = store.sweep_expired(Duration::from_millis(10)).await;
        assert_eq!(swept, 1);
        assert!(!store.contains("old").await);
        assert!(store.contains("fresh").await);
    }

    #[tokio::test]
    async fn remove_returns_the_consumed_entry() {
        let store = ClientCorrelationStore::new();
        store
            .register(
                "e1".into(),
                serde_json::json!("req-1"),
                false,
                Some(serde_json::json!(7)),
            )
            .await;
        let entry = store.remove("e1").await.expect("entry existed");
        assert_eq!(entry.original_id, serde_json::json!("req-1"));
        assert_eq!(entry.progress_token, Some(serde_json::json!(7)));
        assert!(!entry.is_initialize);
        assert!(!store.contains("e1").await);
    }

    #[tokio::test]
    async fn progress_token_preserves_the_exact_json_type() {
        let store = ClientCorrelationStore::new();
        store
            .register(
                "numeric".into(),
                serde_json::json!(1),
                false,
                Some(serde_json::json!(5)),
            )
            .await;
        store
            .register(
                "string".into(),
                serde_json::json!(2),
                false,
                Some(serde_json::json!("5")),
            )
            .await;
        let numeric = store.remove("numeric").await.expect("numeric entry");
        let string = store.remove("string").await.expect("string entry");
        // Number(5) and String("5") stringify identically on the wire but are
        // distinct JSON values; the store must hand back exactly what was sent.
        assert_eq!(numeric.progress_token, Some(serde_json::json!(5)));
        assert_eq!(string.progress_token, Some(serde_json::json!("5")));
        assert_ne!(numeric.progress_token, string.progress_token);
    }

    #[tokio::test]
    async fn touch_refreshes_the_entry_past_a_sweep() {
        let store = ClientCorrelationStore::new();
        store
            .register("kept".into(), serde_json::json!(1), false, None)
            .await;
        store
            .register("dropped".into(), serde_json::json!(2), false, None)
            .await;

        // Age both entries past the timeout, then refresh only one.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(store.touch("kept").await);
        assert!(!store.touch("unknown").await);

        let swept = store.sweep_expired(Duration::from_millis(10)).await;
        assert_eq!(swept, 1, "only the untouched entry expires");
        assert!(store.contains("kept").await);
        assert!(!store.contains("dropped").await);
    }

    #[tokio::test]
    async fn sweep_expired_returns_zero_when_nothing_expired() {
        let store = ClientCorrelationStore::new();
        store
            .register("e1".into(), serde_json::Value::Null, false, None)
            .await;

        let swept = store.sweep_expired(Duration::from_secs(60)).await;
        assert_eq!(swept, 0);
        assert!(store.contains("e1").await);
    }
}
