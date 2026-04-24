//! Conformance tests for store abstractions.
//!
//! Ported from the TS SDK:
//! - `src/transport/nostr-client/correlation-store.test.ts`
//! - `src/transport/nostr-server/session-store.test.ts`
//! - `src/transport/nostr-server/correlation-store.test.ts`
//!
//! LRU eviction tests are deferred — only non-eviction tests are ported here.

use contextvm_sdk::{ClientCorrelationStore, ServerEventRouteStore, SessionStore};
use serde_json::json;

// ════════════════════════════════════════════════════════════════════
// Client Correlation Store
// ════════════════════════════════════════════════════════════════════

mod client_correlation_store {
    use super::*;

    // ── registerRequest ───────────────────────────────────────────

    #[tokio::test]
    async fn stores_request_with_event_id() {
        let store = ClientCorrelationStore::new();
        store.register("event123".into(), json!("req1")).await;
        assert!(store.contains("event123").await);
    }

    #[tokio::test]
    async fn stores_and_resolves_original_request_id() {
        let store = ClientCorrelationStore::new();
        store.register("event456".into(), json!("req2")).await;

        // Retrieve the stored original ID.
        let original = store.get_original_id("event456").await.unwrap();
        assert_eq!(original, json!("req2"));

        // After removal the entry is fully gone.
        assert!(store.remove("event456").await);
        assert!(store.get_original_id("event456").await.is_none());
    }

    // ── resolveResponse (get_original_id + remove) ────────────────

    #[tokio::test]
    async fn restores_original_request_id() {
        let store = ClientCorrelationStore::new();
        store.register("event789".into(), json!(42)).await;
        let original = store.get_original_id("event789").await.unwrap();
        assert_eq!(original, json!(42));
    }

    #[tokio::test]
    async fn returns_none_for_unknown_event_id() {
        let store = ClientCorrelationStore::new();
        assert!(store.get_original_id("unknown").await.is_none());
    }

    #[tokio::test]
    async fn get_and_remove_roundtrip() {
        let store = ClientCorrelationStore::new();
        store.register("event1".into(), json!("req1")).await;

        // Lookup succeeds before removal.
        let original = store.get_original_id("event1").await.unwrap();
        assert_eq!(original, json!("req1"));

        // Remove returns true and cleans up completely.
        assert!(store.remove("event1").await);
        assert!(!store.contains("event1").await);
        assert!(store.get_original_id("event1").await.is_none());
    }

    // ── removePendingRequest ──────────────────────────────────────

    #[tokio::test]
    async fn removes_existing_request() {
        let store = ClientCorrelationStore::new();
        store.register("event1".into(), json!(null)).await;
        assert!(store.remove("event1").await);
        assert!(!store.contains("event1").await);
    }

    #[tokio::test]
    async fn returns_false_for_unknown_request() {
        let store = ClientCorrelationStore::new();
        assert!(!store.remove("unknown").await);
    }

    // ── clear ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn removes_all_pending_requests() {
        let store = ClientCorrelationStore::new();
        store.register("event1".into(), json!(null)).await;
        store.register("event2".into(), json!(null)).await;
        store.clear().await;
        assert_eq!(store.count().await, 0);
    }
}

// ════════════════════════════════════════════════════════════════════
// Server Session Store
// ════════════════════════════════════════════════════════════════════

mod server_session_store {
    use super::*;

    #[tokio::test]
    async fn create_and_retrieve_sessions() {
        let store = SessionStore::new();

        let created = store.get_or_create_session("client-1", true).await;
        assert!(created);

        let session = store.get_session("client-1").await.unwrap();
        assert!(session.is_encrypted);
        assert!(!session.is_initialized);

        // Retrieving same key should return it
        assert!(store.get_session("client-1").await.is_some());
    }

    #[tokio::test]
    async fn mark_sessions_as_initialized() {
        let store = SessionStore::new();
        store.get_or_create_session("client-1", false).await;

        let result = store.mark_initialized("client-1").await;
        assert!(result);

        let session = store.get_session("client-1").await.unwrap();
        assert!(session.is_initialized);
    }

    #[tokio::test]
    async fn remove_sessions() {
        let store = SessionStore::new();
        store.get_or_create_session("client-1", false).await;

        let result = store.remove_session("client-1").await;
        assert!(result);
        assert!(store.get_session("client-1").await.is_none());
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
    async fn iterate_over_all_sessions() {
        let store = SessionStore::new();
        store.get_or_create_session("client-1", false).await;
        store.get_or_create_session("client-2", true).await;

        let sessions = store.get_all_sessions().await;
        assert_eq!(sessions.len(), 2);

        let keys: Vec<&str> = sessions.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"client-1"));
        assert!(keys.contains(&"client-2"));
    }
}

// ════════════════════════════════════════════════════════════════════
// Server Correlation Store (ServerEventRouteStore)
// ════════════════════════════════════════════════════════════════════

mod server_correlation_store {
    use super::*;

    // ── registerEventRoute ────────────────────────────────────────

    #[tokio::test]
    async fn registers_route_with_all_fields() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;

        let route = store.get_route("event1").await.unwrap();
        assert_eq!(route.client_pubkey, "client1");
        assert_eq!(route.original_request_id, json!("req1"));
        assert_eq!(route.progress_token.as_deref(), Some("token1"));
    }

    #[tokio::test]
    async fn registers_route_without_progress_token() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;

        let route = store.get_route("event1").await.unwrap();
        assert!(route.progress_token.is_none());
    }

    #[tokio::test]
    async fn registers_route_with_numeric_request_id() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!(42), None)
            .await;

        let route = store.get_route("event1").await.unwrap();
        assert_eq!(route.original_request_id, json!(42));
    }

    #[tokio::test]
    async fn updates_client_index_when_registering() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;
        store
            .register("event2".into(), "client1".into(), json!("req2"), None)
            .await;

        assert!(store.has_active_routes_for_client("client1").await);
    }

    #[tokio::test]
    async fn registers_progress_token_mapping() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;

        assert_eq!(
            store
                .get_event_id_by_progress_token("token1")
                .await
                .as_deref(),
            Some("event1")
        );
        assert!(store.has_progress_token("token1").await);
    }

    // ── getEventRoute ─────────────────────────────────────────────

    #[tokio::test]
    async fn returns_none_for_unknown_event_id() {
        let store = ServerEventRouteStore::new();
        assert!(store.get_route("unknown").await.is_none());
    }

    // ── popEventRoute ─────────────────────────────────────────────

    #[tokio::test]
    async fn returns_and_removes_route_atomically() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;

        let route = store.pop("event1").await.unwrap();
        assert_eq!(route.client_pubkey, "client1");
        assert_eq!(route.original_request_id, json!("req1"));
        assert_eq!(route.progress_token.as_deref(), Some("token1"));

        // Route + token mapping should be gone.
        assert!(!store.has_event_route("event1").await);
        assert!(!store.has_progress_token("token1").await);

        // Second pop is a no-op.
        assert!(store.pop("event1").await.is_none());
    }

    // ── getEventIdByProgressToken ─────────────────────────────────

    #[tokio::test]
    async fn returns_none_for_unknown_token() {
        let store = ServerEventRouteStore::new();
        assert!(store
            .get_event_id_by_progress_token("unknown")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn returns_correct_event_id_for_token() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;
        store
            .register(
                "event2".into(),
                "client2".into(),
                json!("req2"),
                Some("token2".into()),
            )
            .await;

        assert_eq!(
            store
                .get_event_id_by_progress_token("token1")
                .await
                .as_deref(),
            Some("event1")
        );
        assert_eq!(
            store
                .get_event_id_by_progress_token("token2")
                .await
                .as_deref(),
            Some("event2")
        );
    }

    // ── removeRoutesForClient ─────────────────────────────────────

    #[tokio::test]
    async fn removes_all_routes_for_client() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;
        store
            .register("event2".into(), "client1".into(), json!("req2"), None)
            .await;
        store
            .register("event3".into(), "client2".into(), json!("req3"), None)
            .await;

        let removed = store.remove_for_client("client1").await;
        assert_eq!(removed, 2);

        assert!(!store.has_event_route("event1").await);
        assert!(!store.has_event_route("event2").await);
        assert!(store.has_event_route("event3").await);
    }

    #[tokio::test]
    async fn returns_zero_for_unknown_client() {
        let store = ServerEventRouteStore::new();
        assert_eq!(store.remove_for_client("unknown").await, 0);
    }

    #[tokio::test]
    async fn cleans_up_progress_tokens_for_removed_routes() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;
        store
            .register(
                "event2".into(),
                "client1".into(),
                json!("req2"),
                Some("token2".into()),
            )
            .await;

        store.remove_for_client("client1").await;

        assert!(!store.has_progress_token("token1").await);
        assert!(!store.has_progress_token("token2").await);
    }

    #[tokio::test]
    async fn removes_client_from_index_after_cleanup() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;

        store.remove_for_client("client1").await;

        assert!(!store.has_active_routes_for_client("client1").await);
    }

    // ── hasEventRoute ─────────────────────────────────────────────

    #[tokio::test]
    async fn has_event_route_true_for_existing() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;
        assert!(store.has_event_route("event1").await);
    }

    #[tokio::test]
    async fn has_event_route_false_for_unknown() {
        let store = ServerEventRouteStore::new();
        assert!(!store.has_event_route("unknown").await);
    }

    // ── hasProgressToken ──────────────────────────────────────────

    #[tokio::test]
    async fn has_progress_token_true_for_existing() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;
        assert!(store.has_progress_token("token1").await);
    }

    #[tokio::test]
    async fn has_progress_token_false_for_unknown() {
        let store = ServerEventRouteStore::new();
        assert!(!store.has_progress_token("unknown").await);
    }

    // ── hasActiveRoutesForClient ──────────────────────────────────

    #[tokio::test]
    async fn has_active_routes_true_when_routes_exist() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;
        assert!(store.has_active_routes_for_client("client1").await);
    }

    #[tokio::test]
    async fn has_active_routes_false_when_no_routes() {
        let store = ServerEventRouteStore::new();
        assert!(!store.has_active_routes_for_client("client1").await);
    }

    #[tokio::test]
    async fn has_active_routes_false_after_all_popped() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;
        store.pop("event1").await;
        assert!(!store.has_active_routes_for_client("client1").await);
    }

    // ── eventRouteCount ───────────────────────────────────────────

    #[tokio::test]
    async fn event_route_count_zero_for_empty() {
        let store = ServerEventRouteStore::new();
        assert_eq!(store.event_route_count().await, 0);
    }

    #[tokio::test]
    async fn event_route_count_after_registrations() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;
        store
            .register("event2".into(), "client1".into(), json!("req2"), None)
            .await;
        assert_eq!(store.event_route_count().await, 2);
    }

    #[tokio::test]
    async fn event_route_count_after_removals() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;
        store
            .register("event2".into(), "client1".into(), json!("req2"), None)
            .await;
        store.pop("event1").await;
        assert_eq!(store.event_route_count().await, 1);
    }

    // ── progressTokenCount ────────────────────────────────────────

    #[tokio::test]
    async fn progress_token_count_zero_for_empty() {
        let store = ServerEventRouteStore::new();
        assert_eq!(store.progress_token_count().await, 0);
    }

    #[tokio::test]
    async fn progress_token_count_after_registrations() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;
        store
            .register(
                "event2".into(),
                "client1".into(),
                json!("req2"),
                Some("token2".into()),
            )
            .await;
        store
            .register("event3".into(), "client1".into(), json!("req3"), None)
            .await;
        assert_eq!(store.progress_token_count().await, 2);
    }

    #[tokio::test]
    async fn progress_token_count_after_removals() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;
        store
            .register(
                "event2".into(),
                "client1".into(),
                json!("req2"),
                Some("token2".into()),
            )
            .await;
        store.pop("event1").await;
        assert_eq!(store.progress_token_count().await, 1);
    }

    // ── clear ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn clear_removes_all_routes() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;
        store
            .register("event2".into(), "client2".into(), json!("req2"), None)
            .await;

        store.clear().await;

        assert_eq!(store.event_route_count().await, 0);
        assert!(!store.has_event_route("event1").await);
    }

    #[tokio::test]
    async fn clear_removes_all_progress_tokens() {
        let store = ServerEventRouteStore::new();
        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;

        store.clear().await;

        assert_eq!(store.progress_token_count().await, 0);
        assert!(!store.has_progress_token("token1").await);
    }

    #[tokio::test]
    async fn clear_cleans_up_client_index() {
        let store = ServerEventRouteStore::new();
        store
            .register("event1".into(), "client1".into(), json!("req1"), None)
            .await;

        store.clear().await;

        assert!(!store.has_active_routes_for_client("client1").await);
    }

    // ── complex scenarios ─────────────────────────────────────────

    #[tokio::test]
    async fn handles_multiple_clients_with_multiple_routes() {
        let store = ServerEventRouteStore::new();

        // Client 1: 2 routes
        store
            .register(
                "c1e1".into(),
                "client1".into(),
                json!("r1"),
                Some("t1".into()),
            )
            .await;
        store
            .register(
                "c1e2".into(),
                "client1".into(),
                json!("r2"),
                Some("t2".into()),
            )
            .await;

        // Client 2: 1 route
        store
            .register(
                "c2e1".into(),
                "client2".into(),
                json!("r3"),
                Some("t3".into()),
            )
            .await;

        assert_eq!(store.event_route_count().await, 3);
        assert_eq!(store.progress_token_count().await, 3);
        assert!(store.has_active_routes_for_client("client1").await);
        assert!(store.has_active_routes_for_client("client2").await);

        // Remove one of client1's routes
        store.pop("c1e1").await;

        assert!(store.has_active_routes_for_client("client1").await);
        assert!(!store.has_progress_token("t1").await);
        assert!(store.has_progress_token("t2").await);
    }

    #[tokio::test]
    async fn handles_route_replacement_with_same_progress_token() {
        let store = ServerEventRouteStore::new();

        store
            .register(
                "event1".into(),
                "client1".into(),
                json!("req1"),
                Some("token1".into()),
            )
            .await;
        assert_eq!(
            store
                .get_event_id_by_progress_token("token1")
                .await
                .as_deref(),
            Some("event1")
        );

        // Register new route with same token (overwrites mapping)
        store
            .register(
                "event2".into(),
                "client1".into(),
                json!("req2"),
                Some("token1".into()),
            )
            .await;
        assert_eq!(
            store
                .get_event_id_by_progress_token("token1")
                .await
                .as_deref(),
            Some("event2")
        );
    }

    #[tokio::test]
    async fn maintains_consistency_through_mixed_operations() {
        let store = ServerEventRouteStore::new();

        // Add routes
        store
            .register("e1".into(), "c1".into(), json!("r1"), Some("t1".into()))
            .await;
        store
            .register("e2".into(), "c1".into(), json!("r2"), Some("t2".into()))
            .await;
        store
            .register("e3".into(), "c2".into(), json!("r3"), Some("t3".into()))
            .await;

        // Remove one
        store.pop("e2").await;

        // Verify consistency
        assert!(store.has_event_route("e1").await);
        assert!(!store.has_event_route("e2").await);
        assert!(store.has_event_route("e3").await);

        assert!(store.has_progress_token("t1").await);
        assert!(!store.has_progress_token("t2").await);
        assert!(store.has_progress_token("t3").await);

        assert!(store.has_active_routes_for_client("c1").await);
        assert!(store.has_active_routes_for_client("c2").await);
    }
}
