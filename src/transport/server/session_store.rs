//! Server-side session store for managing client sessions.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::core::types::ClientSession;

/// Manages client sessions keyed by public key (hex).
#[derive(Clone)]
pub struct SessionStore {
    sessions: Arc<RwLock<HashMap<String, ClientSession>>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get an existing session or create a new one. Returns `true` if a new session was created.
    pub async fn get_or_create_session(&self, client_pubkey: &str, is_encrypted: bool) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(client_pubkey) {
            session.is_encrypted = is_encrypted;
            false
        } else {
            sessions.insert(client_pubkey.to_string(), ClientSession::new(is_encrypted));
            true
        }
    }

    /// Get a read-only snapshot of session fields.
    /// Returns `None` if the session does not exist.
    pub async fn get_session(&self, client_pubkey: &str) -> Option<SessionSnapshot> {
        let sessions = self.sessions.read().await;
        sessions.get(client_pubkey).map(|s| SessionSnapshot {
            is_initialized: s.is_initialized,
            is_encrypted: s.is_encrypted,
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

    /// Remove a session. Returns `true` if it existed.
    pub async fn remove_session(&self, client_pubkey: &str) -> bool {
        self.sessions.write().await.remove(client_pubkey).is_some()
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
                    },
                )
            })
            .collect()
    }

    /// Acquire write access to the underlying map (transport internals only).
    pub(crate) async fn write(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, HashMap<String, ClientSession>> {
        self.sessions.write().await
    }

    /// Acquire read access to the underlying map (transport internals only).
    pub(crate) async fn read(
        &self,
    ) -> tokio::sync::RwLockReadGuard<'_, HashMap<String, ClientSession>> {
        self.sessions.read().await
    }
}

/// A lightweight snapshot of session state (avoids exposing the full `ClientSession`
/// through the async API boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub is_initialized: bool,
    pub is_encrypted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_retrieve_session() {
        let store = SessionStore::new();

        let created = store.get_or_create_session("client-1", true).await;
        assert!(created);

        let snap = store.get_session("client-1").await.unwrap();
        assert!(snap.is_encrypted);
        assert!(!snap.is_initialized);
    }

    #[tokio::test]
    async fn get_or_create_returns_existing() {
        let store = SessionStore::new();

        let created = store.get_or_create_session("client-1", false).await;
        assert!(created);

        let created2 = store.get_or_create_session("client-1", true).await;
        assert!(!created2);

        // is_encrypted should have been updated.
        let snap = store.get_session("client-1").await.unwrap();
        assert!(snap.is_encrypted);
    }

    #[tokio::test]
    async fn mark_initialized() {
        let store = SessionStore::new();
        store.get_or_create_session("client-1", false).await;

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
        store.get_or_create_session("client-1", false).await;
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
        store.get_or_create_session("client-1", false).await;
        store.get_or_create_session("client-2", true).await;

        store.clear().await;

        assert_eq!(store.session_count().await, 0);
        assert!(store.get_session("client-1").await.is_none());
        assert!(store.get_session("client-2").await.is_none());
    }

    #[tokio::test]
    async fn get_all_sessions() {
        let store = SessionStore::new();
        store.get_or_create_session("client-1", false).await;
        store.get_or_create_session("client-2", true).await;

        let all = store.get_all_sessions().await;
        assert_eq!(all.len(), 2);

        let keys: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"client-1"));
        assert!(keys.contains(&"client-2"));
    }
}
