//! Client-side correlation store for tracking pending request event IDs.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

/// Tracks pending request event IDs and their original request IDs on the client side.
#[derive(Clone)]
pub struct ClientCorrelationStore {
    pending_requests: Arc<RwLock<HashMap<String, serde_json::Value>>>,
}

impl Default for ClientCorrelationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientCorrelationStore {
    pub fn new() -> Self {
        Self {
            pending_requests: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a pending request with its original JSON-RPC request ID.
    pub async fn register(&self, event_id: String, original_id: serde_json::Value) {
        self.pending_requests
            .write()
            .await
            .insert(event_id, original_id);
    }

    pub async fn contains(&self, event_id: &str) -> bool {
        self.pending_requests.read().await.contains_key(event_id)
    }

    /// Remove a pending request. Returns `true` if the key existed.
    pub async fn remove(&self, event_id: &str) -> bool {
        self.pending_requests
            .write()
            .await
            .remove(event_id)
            .is_some()
    }

    /// Retrieve the original request ID for a given event ID without removing it.
    pub async fn get_original_id(&self, event_id: &str) -> Option<serde_json::Value> {
        self.pending_requests.read().await.get(event_id).cloned()
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
        store.register("e1".into(), serde_json::Value::Null).await;
        store.register("e2".into(), serde_json::Value::Null).await;
        assert!(store.contains("e1").await);
        store.clear().await;
        assert!(!store.contains("e1").await);
        assert!(!store.contains("e2").await);
    }

    #[tokio::test]
    async fn register_and_remove_roundtrip() {
        let store = ClientCorrelationStore::new();
        store.register("e1".into(), serde_json::Value::Null).await;
        assert!(store.contains("e1").await);
        assert!(store.remove("e1").await);
        assert!(!store.contains("e1").await);
    }
}
