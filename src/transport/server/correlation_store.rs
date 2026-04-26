//! Server-side event route store for mapping event IDs to client routes.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;
use tokio::sync::RwLock;

use crate::core::constants::DEFAULT_LRU_SIZE;

/// A route entry for an in-flight request.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    /// The client's public key that originated this request.
    pub client_pubkey: String,
    /// The original JSON-RPC request ID (before replacement with event ID).
    pub original_request_id: serde_json::Value,
    /// Optional progress token for this request.
    pub progress_token: Option<String>,
    /// The outer gift-wrap event kind that carried this request (e.g. 1059 or 21059).
    /// Populated from the inbound event in a later PR; `None` until then.
    pub wrap_kind: Option<u16>,
}

/// Internal state behind the lock.
struct Inner {
    /// Primary index: event_id → route entry (LRU-ordered).
    routes: LruCache<String, RouteEntry>,
    /// Secondary index: progress_token → event_id.
    progress_token_to_event: HashMap<String, String>,
    /// Secondary index: client_pubkey → set of event_ids.
    client_event_ids: HashMap<String, HashSet<String>>,
}

impl Inner {
    fn new(max_routes: usize) -> Self {
        let routes =
            LruCache::new(NonZeroUsize::new(max_routes).expect("max_routes must be non-zero"));
        Self {
            routes,
            progress_token_to_event: HashMap::new(),
            client_event_ids: HashMap::new(),
        }
    }

    /// Clean up secondary indexes for a removed route.
    fn cleanup_indexes(&mut self, event_id: &str, route: &RouteEntry) {
        if let Some(ref token) = route.progress_token {
            self.progress_token_to_event.remove(token);
        }
        if let Some(set) = self.client_event_ids.get_mut(&route.client_pubkey) {
            set.remove(event_id);
            if set.is_empty() {
                self.client_event_ids.remove(&route.client_pubkey);
            }
        }
    }

    /// Remove a single route and clean up all secondary indexes.
    fn remove_route(&mut self, event_id: &str) -> Option<RouteEntry> {
        let route = self.routes.pop(event_id)?;
        self.cleanup_indexes(event_id, &route);
        Some(route)
    }
}

/// Maps event IDs to full route entries for response routing on the server side.
///
/// An optional capacity limit enables LRU eviction; when the limit is reached
/// the oldest entry is evicted and its secondary indexes are cleaned up.
#[derive(Clone)]
pub struct ServerEventRouteStore {
    inner: Arc<RwLock<Inner>>,
}

impl Default for ServerEventRouteStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerEventRouteStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner::new(DEFAULT_LRU_SIZE))),
        }
    }

    /// Create a store with an upper bound on event routes.
    /// When the limit is reached the oldest entry is evicted.
    pub fn with_max_routes(max_routes: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner::new(max_routes))),
        }
    }

    /// Register a route for an incoming request.
    pub async fn register(
        &self,
        event_id: String,
        client_pubkey: String,
        original_request_id: serde_json::Value,
        progress_token: Option<String>,
    ) {
        let mut inner = self.inner.write().await;

        // Update client index.
        inner
            .client_event_ids
            .entry(client_pubkey.clone())
            .or_default()
            .insert(event_id.clone());

        // Update progress token index.
        if let Some(ref token) = progress_token {
            inner
                .progress_token_to_event
                .insert(token.clone(), event_id.clone());
        }

        // Insert into LRU; handle possible eviction.
        let evicted = inner.routes.push(
            event_id.clone(),
            RouteEntry {
                client_pubkey,
                original_request_id,
                progress_token,
                wrap_kind: None,
            },
        );

        if let Some((evicted_key, evicted_route)) = evicted {
            if evicted_key != event_id {
                // A different entry was evicted due to capacity — clean up its indexes.
                inner.cleanup_indexes(&evicted_key, &evicted_route);
            }
        }
    }

    /// Returns the client public key for the given event ID without removing it.
    pub async fn get(&self, event_id: &str) -> Option<String> {
        self.inner
            .read()
            .await
            .routes
            .peek(event_id)
            .map(|r| r.client_pubkey.clone())
    }

    /// Returns the full route entry for the given event ID without removing it.
    pub async fn get_route(&self, event_id: &str) -> Option<RouteEntry> {
        self.inner.read().await.routes.peek(event_id).cloned()
    }

    /// Removes and returns the full route entry for the given event ID.
    pub async fn pop(&self, event_id: &str) -> Option<RouteEntry> {
        self.inner.write().await.remove_route(event_id)
    }

    /// Removes all routes for a given client public key. Returns the count removed.
    pub async fn remove_for_client(&self, client_pubkey: &str) -> usize {
        let mut inner = self.inner.write().await;

        let event_ids = match inner.client_event_ids.remove(client_pubkey) {
            Some(ids) => ids,
            None => return 0,
        };

        let count = event_ids.len();
        for event_id in &event_ids {
            if let Some(route) = inner.routes.pop(event_id.as_str()) {
                if let Some(ref token) = route.progress_token {
                    inner.progress_token_to_event.remove(token);
                }
            }
        }
        count
    }

    /// Check whether a route exists for the given event ID.
    pub async fn has_event_route(&self, event_id: &str) -> bool {
        self.inner.read().await.routes.contains(event_id)
    }

    /// Check whether the given client has any active routes.
    pub async fn has_active_routes_for_client(&self, client_pubkey: &str) -> bool {
        self.inner
            .read()
            .await
            .client_event_ids
            .get(client_pubkey)
            .is_some_and(|set| !set.is_empty())
    }

    /// Look up the event ID associated with a progress token.
    pub async fn get_event_id_by_progress_token(&self, token: &str) -> Option<String> {
        self.inner
            .read()
            .await
            .progress_token_to_event
            .get(token)
            .cloned()
    }

    /// Check whether a progress token mapping exists.
    pub async fn has_progress_token(&self, token: &str) -> bool {
        self.inner
            .read()
            .await
            .progress_token_to_event
            .contains_key(token)
    }

    /// Number of event routes currently tracked.
    pub async fn event_route_count(&self) -> usize {
        self.inner.read().await.routes.len()
    }

    /// Number of progress token mappings currently tracked.
    pub async fn progress_token_count(&self) -> usize {
        self.inner.read().await.progress_token_to_event.len()
    }

    pub async fn clear(&self) {
        let mut inner = self.inner.write().await;
        inner.routes.clear();
        inner.progress_token_to_event.clear();
        inner.client_event_ids.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn pop_on_empty_returns_none() {
        let store = ServerEventRouteStore::new();
        assert!(store.pop("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn get_returns_without_removing() {
        let store = ServerEventRouteStore::new();
        store
            .register("e1".into(), "pk1".into(), json!("r1"), None)
            .await;
        assert_eq!(store.get("e1").await.as_deref(), Some("pk1"));
        assert_eq!(store.get("e1").await.as_deref(), Some("pk1"));
    }

    #[tokio::test]
    async fn pop_removes_entry() {
        let store = ServerEventRouteStore::new();
        store
            .register("e1".into(), "pk1".into(), json!("r1"), None)
            .await;
        let route = store.pop("e1").await.unwrap();
        assert_eq!(route.client_pubkey, "pk1");
        assert!(store.pop("e1").await.is_none());
    }

    #[tokio::test]
    async fn remove_for_client_only_removes_matching() {
        let store = ServerEventRouteStore::new();
        store
            .register("e1".into(), "pk1".into(), json!("r1"), None)
            .await;
        store
            .register("e2".into(), "pk2".into(), json!("r2"), None)
            .await;
        store
            .register("e3".into(), "pk1".into(), json!("r3"), None)
            .await;

        let removed = store.remove_for_client("pk1").await;
        assert_eq!(removed, 2);

        assert!(store.get("e1").await.is_none());
        assert!(store.get("e3").await.is_none());
        assert_eq!(store.get("e2").await.as_deref(), Some("pk2"));
    }

    #[tokio::test]
    async fn remove_for_client_noop_when_no_match() {
        let store = ServerEventRouteStore::new();
        store
            .register("e1".into(), "pk1".into(), json!("r1"), None)
            .await;
        let removed = store.remove_for_client("pk_other").await;
        assert_eq!(removed, 0);
        assert_eq!(store.get("e1").await.as_deref(), Some("pk1"));
    }

    #[tokio::test]
    async fn clear_empties_store() {
        let store = ServerEventRouteStore::new();
        store
            .register("e1".into(), "pk1".into(), json!("r1"), None)
            .await;
        store
            .register("e2".into(), "pk2".into(), json!("r2"), None)
            .await;
        store.clear().await;
        assert!(store.get("e1").await.is_none());
        assert!(store.get("e2").await.is_none());
    }

    #[tokio::test]
    async fn default_store_is_bounded() {
        let store = ServerEventRouteStore::new();
        for i in 0..=DEFAULT_LRU_SIZE {
            store
                .register(format!("e{i}"), "pk1".into(), json!(i), None)
                .await;
        }

        assert_eq!(store.event_route_count().await, DEFAULT_LRU_SIZE);
        assert!(!store.has_event_route("e0").await);
        assert!(store.has_event_route(&format!("e{DEFAULT_LRU_SIZE}")).await);
    }
}
