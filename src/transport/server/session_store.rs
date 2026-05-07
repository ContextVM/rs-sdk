//! Server-side session store for managing client sessions.
//!
//! Uses an LRU cache bounded by `max_sessions` (default 1000, matching the TS SDK
//! server session store).  When a new session would exceed capacity the
//! least-recently-used session is evicted.  If the evicted session still has
//! active routes in the correlation store it is recreated with clean state
//! (eviction safety, matching TS SDK's `hasActiveRoutesForClient` check), and
//! the optional eviction callback fires so external code can clean up resources.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;
use tokio::sync::RwLock;

use crate::core::types::ClientSession;
use crate::transport::server::ServerEventRouteStore;

const LOG_TARGET: &str = "contextvm_sdk::transport::server::session_store";

/// Default maximum number of concurrent client sessions.
///
/// Matches the TS SDK's `SessionStore` default (`maxSessions ?? 1000`), not
/// the broader `DEFAULT_LRU_SIZE` constant (5000) used elsewhere in the TS SDK.
pub const DEFAULT_MAX_SESSIONS: usize = 1000;

/// Callback invoked when a session is evicted from the LRU cache.
/// Receives the evicted client's public key (hex).
pub type EvictionCallback = Arc<dyn Fn(String) + Send + Sync>;

/// Manages client sessions keyed by public key (hex).
///
/// Backed by an LRU cache so memory usage is bounded.
#[derive(Clone)]
pub struct SessionStore {
    sessions: Arc<RwLock<LruCache<String, ClientSession>>>,
    on_evicted: Option<EvictionCallback>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    /// Create a store with the default capacity ([`DEFAULT_MAX_SESSIONS`]).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_SESSIONS)
    }

    /// Create a store with a specific maximum number of sessions.
    pub fn with_capacity(max_sessions: usize) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(max_sessions).unwrap_or(NonZeroUsize::new(1).unwrap()),
            ))),
            on_evicted: None,
        }
    }

    /// Register a callback that fires when a session is evicted from the LRU.
    pub fn set_eviction_callback(&mut self, cb: EvictionCallback) {
        self.on_evicted = Some(cb);
    }

    /// Clone the eviction callback (cheap Arc clone) for use outside the lock.
    pub fn eviction_callback(&self) -> Option<EvictionCallback> {
        self.on_evicted.clone()
    }

    /// Get an existing session or create a new one. Returns `true` if a new session was created.
    ///
    /// `event_routes` is consulted during eviction safety: if the evicted client
    /// still has active routes, the session is recreated with clean state
    /// (matching TS SDK's `hasActiveRoutesForClient` check).
    pub async fn get_or_create_session(
        &self,
        client_pubkey: &str,
        is_encrypted: bool,
        event_routes: &ServerEventRouteStore,
    ) -> bool {
        let on_evicted = self.on_evicted.clone();
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(client_pubkey) {
            session.is_encrypted = is_encrypted;
            false
        } else {
            let new_session = ClientSession::new(is_encrypted);
            let evicted = sessions.push(client_pubkey.to_string(), new_session);
            Self::handle_eviction(
                client_pubkey,
                evicted,
                &mut sessions,
                on_evicted.as_ref(),
                event_routes,
            )
            .await;
            true
        }
    }

    /// Get a read-only snapshot of session fields.
    /// Returns `None` if the session does not exist.
    pub async fn get_session(&self, client_pubkey: &str) -> Option<SessionSnapshot> {
        let sessions = self.sessions.read().await;
        sessions.peek(client_pubkey).map(|s| SessionSnapshot {
            is_initialized: s.is_initialized,
            is_encrypted: s.is_encrypted,
            has_sent_common_tags: s.has_sent_common_tags,
            supports_ephemeral_gift_wrap: s.supports_ephemeral_gift_wrap,
        })
    }

    /// Mark a session as initialized. Returns `true` if the session existed.
    pub async fn mark_initialized(&self, client_pubkey: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(client_pubkey) {
            session.is_initialized = true;
            true
        } else {
            false
        }
    }

    /// Mark that common tags have been sent for this session.
    pub async fn mark_common_tags_sent(&self, client_pubkey: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(client_pubkey) {
            session.has_sent_common_tags = true;
            true
        } else {
            false
        }
    }

    /// Remove a session. Returns `true` if it existed.
    pub async fn remove_session(&self, client_pubkey: &str) -> bool {
        self.sessions.write().await.pop(client_pubkey).is_some()
    }

    /// Remove all sessions.
    pub async fn clear(&self) {
        self.sessions.write().await.clear();
    }

    /// Number of active sessions.
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Return a snapshot of all sessions as `(client_pubkey, snapshot)` pairs.
    pub async fn get_all_sessions(&self) -> Vec<(String, SessionSnapshot)> {
        let sessions = self.sessions.read().await;
        sessions
            .iter()
            .map(|(k, s)| {
                (
                    k.clone(),
                    SessionSnapshot {
                        is_initialized: s.is_initialized,
                        is_encrypted: s.is_encrypted,
                        has_sent_common_tags: s.has_sent_common_tags,
                        supports_ephemeral_gift_wrap: s.supports_ephemeral_gift_wrap,
                    },
                )
            })
            .collect()
    }

    /// Acquire write access to the underlying LRU cache (transport internals only).
    pub(crate) async fn write(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, LruCache<String, ClientSession>> {
        self.sessions.write().await
    }

    /// Acquire read access to the underlying LRU cache (transport internals only).
    pub(crate) async fn read(
        &self,
    ) -> tokio::sync::RwLockReadGuard<'_, LruCache<String, ClientSession>> {
        self.sessions.read().await
    }

    /// Handle a potential LRU eviction after inserting a session.
    ///
    /// If the evicted client still has active routes in the correlation store,
    /// a clean session is re-inserted (eviction safety, matching TS SDK's
    /// `hasActiveRoutesForClient` check).  The eviction callback fires only
    /// for genuine, non-vetoed evictions.
    pub(crate) async fn handle_eviction(
        inserted_key: &str,
        evicted: Option<(String, ClientSession)>,
        sessions: &mut LruCache<String, ClientSession>,
        on_evicted: Option<&EvictionCallback>,
        event_routes: &ServerEventRouteStore,
    ) {
        if let Some((evicted_key, evicted_session)) = evicted {
            // `push` also returns the old value when the *same* key is updated;
            // only act when a *different* key was evicted due to capacity.
            if evicted_key != inserted_key {
                if event_routes
                    .has_active_routes_for_client(&evicted_key)
                    .await
                {
                    tracing::warn!(
                        target: LOG_TARGET,
                        client_pubkey = %evicted_key,
                        "LRU eviction of session with active routes; recreating with clean state"
                    );
                    // Re-insert with clean state so the client isn't orphaned.
                    // Skip the external callback — the session still exists
                    // (matches TS SDK: vetoed evictions don't fire the callback).
                    let _ = sessions.push(
                        evicted_key.clone(),
                        ClientSession::new(evicted_session.is_encrypted),
                    );
                } else if let Some(cb) = on_evicted {
                    cb(evicted_key);
                }
            }
        }
    }
}

/// A lightweight snapshot of session state (avoids exposing the full `ClientSession`
/// through the async API boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub is_initialized: bool,
    pub is_encrypted: bool,
    pub has_sent_common_tags: bool,
    pub supports_ephemeral_gift_wrap: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn routes() -> ServerEventRouteStore {
        ServerEventRouteStore::new()
    }

    #[tokio::test]
    async fn create_and_retrieve_session() {
        let store = SessionStore::new();
        let r = routes();

        let created = store.get_or_create_session("client-1", true, &r).await;
        assert!(created);

        let snap = store.get_session("client-1").await.unwrap();
        assert!(snap.is_encrypted);
        assert!(!snap.is_initialized);
    }

    #[tokio::test]
    async fn get_or_create_returns_existing() {
        let store = SessionStore::new();
        let r = routes();

        let created = store.get_or_create_session("client-1", false, &r).await;
        assert!(created);

        let created2 = store.get_or_create_session("client-1", true, &r).await;
        assert!(!created2);

        let snap = store.get_session("client-1").await.unwrap();
        assert!(snap.is_encrypted);
    }

    #[tokio::test]
    async fn mark_initialized() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;

        assert!(store.mark_initialized("client-1").await);
        let snap = store.get_session("client-1").await.unwrap();
        assert!(snap.is_initialized);
    }

    #[tokio::test]
    async fn mark_initialized_unknown_returns_false() {
        let store = SessionStore::new();
        assert!(!store.mark_initialized("unknown").await);
    }

    #[tokio::test]
    async fn remove_session() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;
        assert!(store.remove_session("client-1").await);
        assert!(store.get_session("client-1").await.is_none());
    }

    #[tokio::test]
    async fn remove_unknown_returns_false() {
        let store = SessionStore::new();
        assert!(!store.remove_session("unknown").await);
    }

    #[tokio::test]
    async fn clear_all_sessions() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;
        store.get_or_create_session("client-2", true, &r).await;

        store.clear().await;

        assert_eq!(store.session_count().await, 0);
        assert!(store.get_session("client-1").await.is_none());
        assert!(store.get_session("client-2").await.is_none());
    }

    #[tokio::test]
    async fn get_all_sessions() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;
        store.get_or_create_session("client-2", true, &r).await;

        let all = store.get_all_sessions().await;
        assert_eq!(all.len(), 2);

        let keys: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"client-1"));
        assert!(keys.contains(&"client-2"));
    }

    // ── CEP-35 capability fields ────────────────────────────────

    #[tokio::test]
    async fn new_session_capability_fields_default_false() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;

        let sessions = store.read().await;
        let session = sessions.peek("client-1").unwrap();
        assert!(!session.has_sent_common_tags);
        assert!(!session.supports_encryption);
        assert!(!session.supports_ephemeral_encryption);
        assert!(!session.supports_oversized_transfer);
    }

    #[tokio::test]
    async fn has_sent_common_tags_flag() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;

        let mut sessions = store.write().await;
        let session = sessions.get_mut("client-1").unwrap();
        assert!(!session.has_sent_common_tags);
        session.has_sent_common_tags = true;
        assert!(session.has_sent_common_tags);
    }

    #[tokio::test]
    async fn capability_or_assign_persists() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;

        {
            let mut sessions = store.write().await;
            let session = sessions.get_mut("client-1").unwrap();
            session.supports_encryption |= true;
            session.supports_ephemeral_encryption |= false;
        }

        {
            let mut sessions = store.write().await;
            let session = sessions.get_mut("client-1").unwrap();
            session.supports_encryption |= false;
            session.supports_ephemeral_encryption |= true;
        }

        let sessions = store.read().await;
        let session = sessions.peek("client-1").unwrap();
        assert!(session.supports_encryption, "OR-assign must not downgrade");
        assert!(session.supports_ephemeral_encryption);
        assert!(!session.supports_oversized_transfer);
    }

    #[tokio::test]
    async fn capability_fields_independent_per_client() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-a", false, &r).await;
        store.get_or_create_session("client-b", false, &r).await;

        {
            let mut sessions = store.write().await;
            let sa = sessions.get_mut("client-a").unwrap();
            sa.supports_encryption = true;
            sa.has_sent_common_tags = true;
        }

        let sessions = store.read().await;
        let sa = sessions.peek("client-a").unwrap();
        let sb = sessions.peek("client-b").unwrap();
        assert!(sa.supports_encryption);
        assert!(sa.has_sent_common_tags);
        assert!(!sb.supports_encryption);
        assert!(!sb.has_sent_common_tags);
    }

    #[tokio::test]
    async fn get_or_create_preserves_capability_fields() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;

        {
            let mut sessions = store.write().await;
            let session = sessions.get_mut("client-1").unwrap();
            session.supports_encryption = true;
            session.has_sent_common_tags = true;
        }

        let created = store.get_or_create_session("client-1", true, &r).await;
        assert!(!created);

        let sessions = store.read().await;
        let session = sessions.peek("client-1").unwrap();
        assert!(session.supports_encryption);
        assert!(session.has_sent_common_tags);
    }

    #[tokio::test]
    async fn clear_resets_capability_fields() {
        let store = SessionStore::new();
        let r = routes();
        store.get_or_create_session("client-1", false, &r).await;
        {
            let mut sessions = store.write().await;
            let s = sessions.get_mut("client-1").unwrap();
            s.supports_encryption = true;
        }

        store.clear().await;
        store.get_or_create_session("client-1", false, &r).await;

        let sessions = store.read().await;
        let session = sessions.peek("client-1").unwrap();
        assert!(!session.supports_encryption);
        assert!(!session.has_sent_common_tags);
    }

    // ── LRU eviction ────────────────────────────────────────────

    #[tokio::test]
    async fn lru_eviction_drops_oldest_session() {
        let store = SessionStore::with_capacity(3);
        let r = routes();
        store.get_or_create_session("a", false, &r).await;
        store.get_or_create_session("b", false, &r).await;
        store.get_or_create_session("c", false, &r).await;

        store.get_or_create_session("d", false, &r).await;

        assert!(
            store.get_session("a").await.is_none(),
            "a should be evicted"
        );
        assert!(store.get_session("b").await.is_some());
        assert!(store.get_session("c").await.is_some());
        assert!(store.get_session("d").await.is_some());
        assert_eq!(store.session_count().await, 3);
    }

    #[tokio::test]
    async fn eviction_callback_fires_on_lru_eviction() {
        let evicted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let evicted_clone = evicted.clone();
        let r = routes();

        let mut store = SessionStore::with_capacity(2);
        store.set_eviction_callback(Arc::new(move |pubkey| {
            evicted_clone.lock().unwrap().push(pubkey);
        }));

        store.get_or_create_session("a", false, &r).await;
        store.get_or_create_session("b", false, &r).await;
        store.get_or_create_session("c", false, &r).await;

        let evicted = evicted.lock().unwrap();
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0], "a");
    }

    #[tokio::test]
    async fn eviction_safety_recreates_session_with_active_routes() {
        let store = SessionStore::with_capacity(2);
        let r = routes();
        store.get_or_create_session("a", true, &r).await;
        store.get_or_create_session("b", false, &r).await;

        // Register an active route for client "a" in the correlation store
        r.register("evt1".into(), "a".into(), json!(1), None).await;

        // Adding "c" would normally evict "a", but eviction safety recreates it
        // because "a" has active routes.
        store.get_or_create_session("c", false, &r).await;

        let snap = store.get_session("a").await;
        assert!(
            snap.is_some(),
            "session with active routes must survive eviction"
        );
        // "b" was evicted instead (next LRU after "a" was re-inserted)
        assert!(
            store.get_session("b").await.is_none(),
            "b should be evicted"
        );
    }

    #[tokio::test]
    async fn with_capacity_sets_limit() {
        let store = SessionStore::with_capacity(5);
        let r = routes();
        for i in 0..10 {
            store
                .get_or_create_session(&format!("client-{i}"), false, &r)
                .await;
        }
        assert_eq!(store.session_count().await, 5);
    }
}
