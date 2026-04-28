//! Client-side correlation store for tracking pending request event IDs.

use std::num::NonZeroUsize;
use std::sync::Arc;

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
    pub fn new() -> Self {
        Self::with_max_pending(DEFAULT_LRU_SIZE)
    }

    /// Create a store with an upper bound on pending requests.
    /// When the limit is reached the oldest entry is evicted.
    pub fn with_max_pending(max_pending: usize) -> Self {
        Self {
            pending_requests: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(max_pending).expect("max_pending must be non-zero"),
            ))),
        }
    }

    /// Register a pending request with its original JSON-RPC request ID.
    pub async fn register(
        &self,
        event_id: String,
        original_id: serde_json::Value,
        is_initialize: bool,
    ) {
        self.pending_requests.write().await.push(
            event_id,
            PendingRequest {
                original_id,
                is_initialize,
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

    pub async fn contains(&self, event_id: &str) -> bool {
        self.pending_requests.read().await.contains(event_id)
    }

    /// Remove a pending request. Returns `true` if the key existed.
    pub async fn remove(&self, event_id: &str) -> bool {
        self.pending_requests.write().await.pop(event_id).is_some()
    }

    /// Retrieve the original request ID for a given event ID without removing it.
    pub async fn get_original_id(&self, event_id: &str) -> Option<serde_json::Value> {
        self.pending_requests
            .read()
            .await
            .peek(event_id)
            .map(|r| r.original_id.clone())
    }

    /// Number of pending requests currently tracked.
    pub async fn count(&self) -> usize {
        self.pending_requests.read().await.len()
    }

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
        assert!(!store.remove("nonexistent").await);
        assert!(!store.contains("nonexistent").await);
    }

    #[tokio::test]
    async fn contains_after_clear() {
        let store = ClientCorrelationStore::new();
        store
            .register("e1".into(), serde_json::Value::Null, false)
            .await;
        store
            .register("e2".into(), serde_json::Value::Null, false)
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
            .register("e1".into(), serde_json::Value::Null, false)
            .await;
        assert!(store.contains("e1").await);
        assert!(store.remove("e1").await);
        assert!(!store.contains("e1").await);
    }

    #[tokio::test]
    async fn default_store_is_bounded() {
        let store = ClientCorrelationStore::new();
        for i in 0..=DEFAULT_LRU_SIZE {
            store
                .register(format!("e{i}"), serde_json::Value::Null, false)
                .await;
        }

        assert_eq!(store.count().await, DEFAULT_LRU_SIZE);
        assert!(!store.contains("e0").await);
        assert!(store.contains(&format!("e{DEFAULT_LRU_SIZE}")).await);
    }
}
