//! Integration tests — transport-level flows using MockRelayPool.
//!
//! Each test wires client and/or server transports to an in-memory mock relay
//! network so that the full event-loop logic (subscription, publish, routing,
//! encryption-mode enforcement, and authorization) is exercised without
//! connecting to real relays.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use contextvm_sdk::core::constants::{
    mcp_protocol_version, CTXVM_MESSAGES_KIND, GIFT_WRAP_KIND, PROMPTS_LIST_KIND,
    RESOURCES_LIST_KIND, RESOURCETEMPLATES_LIST_KIND, SERVER_ANNOUNCEMENT_KIND, TOOLS_LIST_KIND,
};
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use contextvm_sdk::{
    CapabilityExclusion, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    RelayPoolTrait, ServerInfo,
};
use nostr_sdk::prelude::*;

fn as_pool(pool: MockRelayPool) -> Arc<dyn RelayPoolTrait> {
    Arc::new(pool)
}

struct TestRelayPool {
    inner: Arc<MockRelayPool>,
    publish_delay: Duration,
    failures_remaining: AtomicUsize,
    publish_attempts: AtomicUsize,
}

impl TestRelayPool {
    fn with_publish_delay(inner: Arc<MockRelayPool>, publish_delay: Duration) -> Self {
        Self {
            inner,
            publish_delay,
            failures_remaining: AtomicUsize::new(0),
            publish_attempts: AtomicUsize::new(0),
        }
    }

    fn with_publish_failures(inner: Arc<MockRelayPool>, failures: usize) -> Self {
        Self {
            inner,
            publish_delay: Duration::ZERO,
            failures_remaining: AtomicUsize::new(failures),
            publish_attempts: AtomicUsize::new(0),
        }
    }

    fn publish_attempts(&self) -> usize {
        self.publish_attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RelayPoolTrait for TestRelayPool {
    async fn connect(&self, relay_urls: &[String]) -> contextvm_sdk::Result<()> {
        self.inner.connect(relay_urls).await
    }

    async fn disconnect(&self) -> contextvm_sdk::Result<()> {
        self.inner.disconnect().await
    }

    async fn publish_event(&self, event: &Event) -> contextvm_sdk::Result<EventId> {
        if !self.publish_delay.is_zero() {
            tokio::time::sleep(self.publish_delay).await;
        }
        self.publish_attempts.fetch_add(1, Ordering::SeqCst);
        let should_fail = self
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();

        if should_fail {
            return Err(contextvm_sdk::Error::Transport(
                "injected publish failure".to_string(),
            ));
        }

        self.inner.publish_event(event).await
    }

    async fn publish(&self, builder: EventBuilder) -> contextvm_sdk::Result<EventId> {
        if !self.publish_delay.is_zero() {
            tokio::time::sleep(self.publish_delay).await;
        }
        self.inner.publish(builder).await
    }

    async fn sign(&self, builder: EventBuilder) -> contextvm_sdk::Result<Event> {
        self.inner.sign(builder).await
    }

    async fn signer(&self) -> contextvm_sdk::Result<Arc<dyn NostrSigner>> {
        self.inner.signer().await
    }

    fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
        self.inner.notifications()
    }

    async fn public_key(&self) -> contextvm_sdk::Result<PublicKey> {
        self.inner.public_key().await
    }

    async fn subscribe(&self, filters: Vec<Filter>) -> contextvm_sdk::Result<()> {
        self.inner.subscribe(filters).await
    }
}

/// Let spawned event loops call `notifications()` before we publish anything.
/// Without this, broadcast messages can be lost on slow CI runners.
async fn let_event_loops_start() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

// ── 1. Full initialization handshake ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_initialization_handshake() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends initialize request.
    let init_request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.0.0" }
        })),
    });
    client
        .send(&init_request)
        .await
        .expect("client send initialize");

    // Server should receive the initialize request.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive init request")
        .expect("server channel closed");

    assert_eq!(
        incoming.message.method(),
        Some("initialize"),
        "server must receive initialize request"
    );

    // Server sends initialize response.
    let init_response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        result: serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "serverInfo": { "name": "test-server", "version": "0.0.0" },
            "capabilities": {}
        }),
    });
    server
        .send_response(&incoming.event_id, init_response)
        .await
        .expect("server send response");

    // Client should receive the initialize response.
    let response = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to receive init response")
        .expect("client channel closed");

    assert!(response.is_response(), "client must receive a response");
    assert_eq!(response.id(), Some(&serde_json::json!(1)));
}

// ── 2. Server announcement publishing ───────────────────────────────────────

#[tokio::test]
async fn server_announcement_publishing() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            is_announced_server: true,
            server_info: Some(ServerInfo {
                name: Some("Phase3-Test-Server".to_string()),
                ..Default::default()
            }),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");
    server.announce().await.expect("server announce");

    let events = pool.stored_events().await;
    let announcement = events
        .iter()
        .find(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND));

    assert!(
        announcement.is_some(),
        "kind {} event must be published after announce()",
        SERVER_ANNOUNCEMENT_KIND
    );

    let ann = announcement.unwrap();
    let content: serde_json::Value =
        serde_json::from_str(&ann.content).expect("announcement content must be JSON");
    assert_eq!(
        content["name"], "Phase3-Test-Server",
        "announcement content must include server name"
    );
}

// ── 3. Encryption mode Optional accepts plaintext ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encryption_mode_optional_accepts_plaintext() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Server uses Optional — should accept both encrypted and plaintext.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Optional,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    // Client uses Disabled — sends plaintext kind 25910.
    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("plain-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send plaintext request");

    // Server must receive and process the plaintext message.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive plaintext request")
        .expect("server channel closed");

    assert_eq!(
        incoming.message.method(),
        Some("tools/list"),
        "Optional-mode server must accept plaintext kind 25910"
    );
    assert!(
        !incoming.is_encrypted,
        "plaintext request must not be marked as encrypted"
    );
}

// ── 4. Auth allowlist blocks disallowed pubkey ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_allowlist_blocks_disallowed_pubkey() {
    let allowed_keys = Keys::generate(); // a DIFFERENT pubkey
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Server allows only `allowed_keys` — client_keys is NOT allowed.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            allowed_public_keys: vec![allowed_keys.public_key().to_hex()],
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Send a non-initialize request (those are always allowed).
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(42),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // The server should NOT forward the request (pubkey is disallowed).
    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "disallowed pubkey request must not reach the server handler"
    );
}

// ── 5. Encryption mode Required drops plaintext ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encryption_mode_required_drops_plaintext() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Server requires encryption — plaintext must be dropped.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    // Client sends plaintext (Disabled mode).
    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("drop-me"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send plaintext request");

    // Server must NOT receive the plaintext message.
    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "Required-mode server must drop plaintext kind 25910 events"
    );
}

// ── 6. Encrypted gift-wrap roundtrip ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_gift_wrap_roundtrip() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends encrypted request.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("enc-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send encrypted request");

    // Verify the published event is a gift-wrap (kind 1059).
    let events = server_pool.stored_events().await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND)),
        "client must publish a kind 1059 gift-wrap event"
    );

    // Server should decrypt and receive the request.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to decrypt gift-wrap request")
        .expect("server channel closed");

    assert_eq!(incoming.message.method(), Some("tools/list"));
    assert!(incoming.is_encrypted, "message must be marked encrypted");

    // Server sends an encrypted response back.
    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("enc-1"),
        result: serde_json::json!({ "tools": [] }),
    });
    server
        .send_response(&incoming.event_id, response)
        .await
        .expect("server send encrypted response");

    // Client should decrypt and receive the response.
    let client_msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to decrypt gift-wrap response")
        .expect("client channel closed");

    assert!(client_msg.is_response());
    assert_eq!(client_msg.id(), Some(&serde_json::json!("enc-1")));
}

// ── 7. Gift-wrap dedup skips duplicate delivery ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gift_wrap_dedup_skips_duplicate_delivery() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends a gift-wrapped request.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("dedup-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // Server receives the first delivery.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for first delivery")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/list"));
    assert!(incoming.is_encrypted);

    // Re-deliver the same gift-wrap event (simulates relay redelivery).
    let events = server_pool.stored_events().await;
    let gift_wrap = events
        .iter()
        .find(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND))
        .expect("gift-wrap event must exist")
        .clone();
    server_pool
        .publish_event(&gift_wrap)
        .await
        .expect("re-inject duplicate");

    // Server must NOT process the duplicate.
    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "duplicate gift-wrap (same outer event id) must be skipped"
    );
}

// ── 8. Correlated notification has e tag ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correlated_notification_has_e_tag() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends a tools/list request.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("notif-corr"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // Server receives the request and captures the event_id.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive request")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/list"));
    let request_event_id = incoming.event_id.clone();

    // Server sends a correlated notifications/progress notification.
    let notification = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/progress".to_string(),
        params: Some(serde_json::json!({
            "progressToken": "tok-1",
            "progress": 50,
            "total": 100
        })),
    });
    server
        .send_notification(
            &incoming.client_pubkey,
            &notification,
            Some(&request_event_id),
        )
        .await
        .expect("send correlated notification");

    // Client should receive the notification.
    let client_msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to receive notification")
        .expect("client channel closed");

    assert!(client_msg.is_notification());
    assert_eq!(client_msg.method(), Some("notifications/progress"));

    // The published notification event must carry an e tag referencing the request.
    let events = server_pool.stored_events().await;
    let notif_event = events
        .iter()
        .find(|e| e.pubkey == server_pubkey && e.content.contains("notifications/progress"))
        .expect("notification event must be in stored events");

    let e_tag = contextvm_sdk::core::serializers::get_tag_value(&notif_event.tags, "e");
    assert_eq!(
        e_tag.as_deref(),
        Some(request_event_id.as_str()),
        "notification event must have e tag referencing the original request event id"
    );
}

// ── 9. Encryption Required client, Optional server ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encryption_required_client_optional_server() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Optional,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("enc-opt-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send encrypted request");

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive encrypted request")
        .expect("server channel closed");

    assert_eq!(
        incoming.message.method(),
        Some("tools/list"),
        "Optional-mode server must accept encrypted messages from Required-mode client"
    );
    assert!(
        incoming.is_encrypted,
        "message from Required-mode client must be marked encrypted"
    );
}

// ── 10. Encryption Optional both sides, encrypted path ──────────────────────
// Optional client defaults to encrypting (unwrap_or(true)), Optional server
// accepts encrypted messages. Tests the Optional/Optional negotiation path.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encryption_optional_both_sides_encrypted_path() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Optional,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Optional,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("opt-both-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive request")
        .expect("server channel closed");

    assert_eq!(incoming.message.method(), Some("tools/list"));
    assert!(
        incoming.is_encrypted,
        "Optional client defaults to encrypting; Optional server must accept"
    );
}

// ── 11. Announce includes encryption tags ────────────────────────────────────

#[tokio::test]
async fn announce_includes_encryption_tags() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            is_announced_server: true,
            server_info: Some(ServerInfo {
                name: Some("Encrypted-Server".to_string()),
                ..Default::default()
            }),
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");
    server.announce().await.expect("server announce");

    let events = pool.stored_events().await;
    let announcement = events
        .iter()
        .find(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND))
        .expect("kind 11316 event must be published");

    // support_encryption is a valueless tag — check tag name directly.
    let has_support_encryption = announcement
        .tags
        .iter()
        .any(|t| t.clone().to_vec().first().map(|s| s.as_str()) == Some("support_encryption"));
    let has_support_encryption_ephemeral = announcement.tags.iter().any(|t| {
        t.clone().to_vec().first().map(|s| s.as_str()) == Some("support_encryption_ephemeral")
    });

    assert!(
        has_support_encryption,
        "announcement must include support_encryption tag"
    );
    assert!(
        has_support_encryption_ephemeral,
        "announcement must include support_encryption_ephemeral tag"
    );
}

// ── 12. Announce includes server metadata tags ──────────────────────────────

#[tokio::test]
async fn announce_includes_server_metadata_tags() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            is_announced_server: true,
            server_info: Some(ServerInfo {
                name: Some("Meta-Server".to_string()),
                about: Some("A test server".to_string()),
                website: Some("https://example.com".to_string()),
                picture: Some("https://example.com/pic.png".to_string()),
                ..Default::default()
            }),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");
    server.announce().await.expect("server announce");

    let events = pool.stored_events().await;
    let announcement = events
        .iter()
        .find(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND))
        .expect("kind 11316 event must be published");

    let name_tag = contextvm_sdk::core::serializers::get_tag_value(&announcement.tags, "name");
    let about_tag = contextvm_sdk::core::serializers::get_tag_value(&announcement.tags, "about");
    let website_tag =
        contextvm_sdk::core::serializers::get_tag_value(&announcement.tags, "website");
    let picture_tag =
        contextvm_sdk::core::serializers::get_tag_value(&announcement.tags, "picture");

    assert_eq!(
        name_tag.as_deref(),
        Some("Meta-Server"),
        "name tag must be present"
    );
    assert_eq!(
        about_tag.as_deref(),
        Some("A test server"),
        "about tag must be present"
    );
    assert_eq!(
        website_tag.as_deref(),
        Some("https://example.com"),
        "website tag must be present"
    );
    assert_eq!(
        picture_tag.as_deref(),
        Some("https://example.com/pic.png"),
        "picture tag must be present"
    );
}

// ── 13. Publish tools produces correct kind ─────────────────────────────────

#[tokio::test]
async fn publish_tools_produces_correct_kind() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            is_announced_server: true,
            server_info: Some(ServerInfo {
                name: Some("Tools-Server".to_string()),
                ..Default::default()
            }),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");
    server.announce().await.expect("server announce");

    let tools = vec![serde_json::json!({
        "name": "get_weather",
        "description": "Get the weather",
        "inputSchema": { "type": "object" }
    })];
    server.publish_tools(tools).await.expect("publish tools");

    let events = pool.stored_events().await;
    let tools_event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(TOOLS_LIST_KIND))
        .expect("kind 11317 event must be published");

    let content: serde_json::Value =
        serde_json::from_str(&tools_event.content).expect("tools content must be JSON");
    assert!(
        content.get("tools").is_some(),
        "tools event content must contain 'tools' key"
    );
    let tools_arr = content["tools"].as_array().expect("tools must be an array");
    assert_eq!(tools_arr.len(), 1);
    assert_eq!(tools_arr[0]["name"], "get_weather");
}

// ── 14. Broadcast notification reaches initialized client ─────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_notification_reaches_initialized_client() {
    let (c1_pool, s_pool) = MockRelayPool::create_pair();
    let server_pk = s_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(s_pool),
    )
    .await
    .expect("create server transport");

    let mut srv_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pk.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(c1_pool),
    )
    .await
    .expect("create client transport");
    let mut c_rx = client
        .take_message_receiver()
        .expect("client message receiver");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends initialize request.
    let init_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": { "name": "c1", "version": "0.0.0" }
        })),
    });
    client
        .send(&init_req)
        .await
        .expect("client send initialize");

    let incoming = tokio::time::timeout(Duration::from_millis(500), srv_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");

    // Server responds to initialize.
    let init_resp = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        result: serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "serverInfo": { "name": "test-server", "version": "0.0.0" },
            "capabilities": {}
        }),
    });
    server
        .send_response(&incoming.event_id, init_resp)
        .await
        .expect("send init response");

    // Client receives the init response.
    let _ = tokio::time::timeout(Duration::from_millis(500), c_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");

    // Client sends notifications/initialized → session becomes initialized.
    let init_notif = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/initialized".to_string(),
        params: None,
    });
    client
        .send(&init_notif)
        .await
        .expect("send initialized notification");

    // Drain srv_rx until we see notifications/initialized (skipping any
    // echoed events from the shared mock relay broadcast channel).
    loop {
        let msg = tokio::time::timeout(Duration::from_millis(500), srv_rx.recv())
            .await
            .expect("timeout waiting for notifications/initialized on server")
            .expect("server channel closed");
        if msg.message.method() == Some("notifications/initialized") {
            break;
        }
    }

    // Now broadcast — only the initialized client session should receive it.
    let broadcast = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/progress".to_string(),
        params: Some(serde_json::json!({ "progressToken": "bc-1", "progress": 1, "total": 1 })),
    });
    server
        .broadcast_notification(&broadcast)
        .await
        .expect("broadcast notification");

    let msg = tokio::time::timeout(Duration::from_millis(500), c_rx.recv())
        .await
        .expect("timeout waiting for client to receive broadcast")
        .expect("client channel closed");

    assert_eq!(msg.method(), Some("notifications/progress"));
}

// ── 15. Uncorrelated notification passes through ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncorrelated_notification_passes_through() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    let init_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("unc-init"),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": { "name": "unc-test", "version": "0.0.0" }
        })),
    });
    client.send(&init_req).await.expect("send initialize");

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");

    let init_resp = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("unc-init"),
        result: serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "serverInfo": { "name": "test", "version": "0.0.0" },
            "capabilities": {}
        }),
    });
    server
        .send_response(&incoming.event_id, init_resp)
        .await
        .expect("send init response");

    let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");

    // Uncorrelated notification (no e tag) must pass through to client.
    let notification = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/progress".to_string(),
        params: Some(serde_json::json!({ "progressToken": "unc-1", "progress": 50, "total": 100 })),
    });
    server
        .send_notification(&incoming.client_pubkey, &notification, None)
        .await
        .expect("send uncorrelated notification");

    let client_msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to receive notification")
        .expect("client channel closed");

    assert!(client_msg.is_notification());
    assert_eq!(client_msg.method(), Some("notifications/progress"));
}

// ── 16. Correlated notification with unknown e tag is dropped ───────────────
// NOTE: The Rust SDK drops ANY server event whose e-tag references an unknown
// pending request, including notifications. The TS SDK may forward such events.
// This test documents the Rust SDK's stricter correlation enforcement.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correlated_notification_unknown_e_tag_is_dropped() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    let init_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("corr-init"),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": { "name": "corr-test", "version": "0.0.0" }
        })),
    });
    client.send(&init_req).await.expect("send initialize");

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");

    let init_resp = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("corr-init"),
        result: serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "serverInfo": { "name": "test", "version": "0.0.0" },
            "capabilities": {}
        }),
    });
    server
        .send_response(&incoming.event_id, init_resp)
        .await
        .expect("send init response");

    let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");

    // Notification with e tag referencing unknown event id must be dropped.
    let fake_event_id = "a".repeat(64);
    let notification = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/progress".to_string(),
        params: Some(serde_json::json!({ "progressToken": "fake", "progress": 1, "total": 1 })),
    });
    server
        .send_notification(&incoming.client_pubkey, &notification, Some(&fake_event_id))
        .await
        .expect("send notification with unknown e tag");

    let result = tokio::time::timeout(Duration::from_millis(500), client_rx.recv()).await;
    assert!(
        result.is_err(),
        "notification with unknown e tag must be dropped by client"
    );
}

// ── 17. Auth: allowed pubkey receives response ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_allowed_pubkey_receives_response() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            allowed_public_keys: vec![client_pubkey.to_hex()],
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("auth-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // Server should receive it (pubkey is in the allowlist).
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive request")
        .expect("server channel closed");

    assert_eq!(incoming.message.method(), Some("tools/list"));

    // Server sends response back.
    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("auth-1"),
        result: serde_json::json!({ "tools": [] }),
    });
    server
        .send_response(&incoming.event_id, response)
        .await
        .expect("send response");

    // Client should receive the response.
    let client_msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to receive response")
        .expect("client channel closed");

    assert!(client_msg.is_response());
    assert_eq!(client_msg.id(), Some(&serde_json::json!("auth-1")));
}

// ── 18. Excluded capability bypasses auth ───────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn excluded_capability_bypasses_auth() {
    let allowed_keys = Keys::generate(); // a DIFFERENT pubkey, NOT the client
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            allowed_public_keys: vec![allowed_keys.public_key().to_hex()],
            excluded_capabilities: vec![CapabilityExclusion {
                method: "tools/list".to_string(),
                name: None,
            }],
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client's pubkey is NOT in the allowlist, but "tools/list" is excluded from auth.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("excl-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // Server should receive it because the method is in excluded_capabilities.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive excluded-capability request")
        .expect("server channel closed");

    assert_eq!(
        incoming.message.method(),
        Some("tools/list"),
        "excluded capability must bypass auth allowlist"
    );
}

// ── 19. Publish resources produces correct kind ─────────────────────────────

#[tokio::test]
async fn publish_resources_produces_correct_kind() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");

    let resources = vec![serde_json::json!({
        "uri": "file:///readme.md",
        "name": "readme",
        "mimeType": "text/markdown"
    })];
    server
        .publish_resources(resources)
        .await
        .expect("publish resources");

    let events = pool.stored_events().await;
    let event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(RESOURCES_LIST_KIND))
        .expect("kind 11318 event must be published");

    let content: serde_json::Value =
        serde_json::from_str(&event.content).expect("content must be JSON");
    let arr = content["resources"]
        .as_array()
        .expect("resources must be an array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "readme");
}

// ── 20. Publish prompts produces correct kind ───────────────────────────────

#[tokio::test]
async fn publish_prompts_produces_correct_kind() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");

    let prompts = vec![serde_json::json!({
        "name": "summarize",
        "description": "Summarize text"
    })];
    server
        .publish_prompts(prompts)
        .await
        .expect("publish prompts");

    let events = pool.stored_events().await;
    let event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(PROMPTS_LIST_KIND))
        .expect("kind 11320 event must be published");

    let content: serde_json::Value =
        serde_json::from_str(&event.content).expect("content must be JSON");
    let arr = content["prompts"]
        .as_array()
        .expect("prompts must be an array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "summarize");
}

// ── 21. Publish resource templates produces correct kind ────────────────────

#[tokio::test]
async fn publish_resource_templates_produces_correct_kind() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");

    let templates = vec![serde_json::json!({
        "uriTemplate": "file:///{path}",
        "name": "file",
        "mimeType": "application/octet-stream"
    })];
    server
        .publish_resource_templates(templates)
        .await
        .expect("publish resource templates");

    let events = pool.stored_events().await;
    let event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(RESOURCETEMPLATES_LIST_KIND))
        .expect("kind 11319 event must be published");

    let content: serde_json::Value =
        serde_json::from_str(&event.content).expect("content must be JSON");
    let arr = content["resourceTemplates"]
        .as_array()
        .expect("resourceTemplates must be an array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "file");
}

// ── 22. Publish tools with empty list ───────────────────────────────────────

#[tokio::test]
async fn publish_tools_empty_list() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");
    server
        .publish_tools(vec![])
        .await
        .expect("publish empty tools");

    let events = pool.stored_events().await;
    let event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(TOOLS_LIST_KIND))
        .expect("kind 11317 event must be published for empty list");

    let content: serde_json::Value =
        serde_json::from_str(&event.content).expect("content must be JSON");
    let arr = content["tools"].as_array().expect("tools must be an array");
    assert!(arr.is_empty(), "empty tools list must produce tools: []");
}

// ── 23. Delete announcements k tags match kinds ─────────────────────────────

#[tokio::test]
async fn delete_announcements_k_tags_match_kinds() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            is_announced_server: true,
            server_info: Some(ServerInfo {
                name: Some("KTag-Server".to_string()),
                ..Default::default()
            }),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");
    server.announce().await.expect("server announce");
    server
        .delete_announcements("shutting down")
        .await
        .expect("delete announcements");

    let events = pool.stored_events().await;
    let kind5_events: Vec<_> = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(5))
        .collect();

    assert_eq!(kind5_events.len(), 5);

    // Collect k tag values from all kind-5 events.
    let mut k_values: Vec<u16> = kind5_events
        .iter()
        .filter_map(|e| {
            contextvm_sdk::core::serializers::get_tag_value(&e.tags, "k")
                .and_then(|v| v.parse::<u16>().ok())
        })
        .collect();
    k_values.sort();

    let mut expected = vec![
        SERVER_ANNOUNCEMENT_KIND,
        TOOLS_LIST_KIND,
        RESOURCES_LIST_KIND,
        RESOURCETEMPLATES_LIST_KIND,
        PROMPTS_LIST_KIND,
    ];
    expected.sort();

    assert_eq!(
        k_values, expected,
        "each kind-5 event must have a k tag matching one announcement kind"
    );
}

// ── 24. Encryption Disabled server rejects gift-wrap ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encryption_disabled_server_rejects_gift_wrap() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Server has encryption disabled — must reject gift-wrap events.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    // Client requires encryption — sends gift-wrap (kind 1059).
    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("gw-reject"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send encrypted request");

    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "Disabled-mode server must drop gift-wrap events"
    );
}

// ── 25. Response mirrors client encryption format ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_mirrors_client_encryption_format() {
    // Part A: Disabled client → Optional server → response must be plaintext (kind 25910).
    {
        let (client_pool, server_pool) = MockRelayPool::create_pair();
        let server_pubkey = server_pool.mock_public_key();
        let server_pool = Arc::new(server_pool);

        let mut server = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig {
                encryption_mode: EncryptionMode::Optional,
                ..Default::default()
            },
            Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("create server transport");

        let mut client = NostrClientTransport::with_relay_pool(
            NostrClientTransportConfig {
                server_pubkey: server_pubkey.to_hex(),
                encryption_mode: EncryptionMode::Disabled,
                ..Default::default()
            },
            as_pool(client_pool),
        )
        .await
        .expect("create client transport");

        let mut server_rx = server
            .take_message_receiver()
            .expect("server message receiver");
        let mut client_rx = client
            .take_message_receiver()
            .expect("client message receiver");

        server.start().await.expect("server start");
        client.start().await.expect("client start");
        let_event_loops_start().await;

        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("mirror-plain"),
            method: "tools/list".to_string(),
            params: None,
        });
        client.send(&request).await.expect("send plaintext request");

        let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(!incoming.is_encrypted);

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("mirror-plain"),
            result: serde_json::json!({ "tools": [] }),
        });
        server
            .send_response(&incoming.event_id, response)
            .await
            .expect("send plaintext response");

        // Client receives the response.
        let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        // Verify response event is plaintext kind 25910, not gift-wrap.
        let events = server_pool.stored_events().await;
        let response_events: Vec<_> = events
            .iter()
            .filter(|e| e.pubkey == server_pubkey && e.content.contains("mirror-plain"))
            .collect();
        assert!(
            !response_events.is_empty(),
            "server must publish a response event"
        );
        assert!(
            response_events
                .iter()
                .all(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)),
            "response to plaintext client must be kind {} (plaintext)",
            CTXVM_MESSAGES_KIND
        );
    }

    // Part B: Required client → Optional server → response must be gift-wrap (kind 1059).
    {
        let (client_pool, server_pool) = MockRelayPool::create_pair();
        let server_pubkey = server_pool.mock_public_key();
        let server_pool = Arc::new(server_pool);

        let mut server = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig {
                encryption_mode: EncryptionMode::Optional,
                ..Default::default()
            },
            Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("create server transport");

        let mut client = NostrClientTransport::with_relay_pool(
            NostrClientTransportConfig {
                server_pubkey: server_pubkey.to_hex(),
                encryption_mode: EncryptionMode::Required,
                ..Default::default()
            },
            as_pool(client_pool),
        )
        .await
        .expect("create client transport");

        let mut server_rx = server
            .take_message_receiver()
            .expect("server message receiver");
        let mut client_rx = client
            .take_message_receiver()
            .expect("client message receiver");

        server.start().await.expect("server start");
        client.start().await.expect("client start");
        let_event_loops_start().await;

        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("mirror-enc"),
            method: "tools/list".to_string(),
            params: None,
        });
        client.send(&request).await.expect("send encrypted request");

        let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(incoming.is_encrypted);

        // Snapshot gift-wrap count before server responds.
        let gw_before = server_pool
            .stored_events()
            .await
            .iter()
            .filter(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND))
            .count();

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("mirror-enc"),
            result: serde_json::json!({ "tools": [] }),
        });
        server
            .send_response(&incoming.event_id, response)
            .await
            .expect("send encrypted response");

        // Client receives the response.
        let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        // Verify server published exactly one new gift-wrap for the response.
        let gw_after = server_pool
            .stored_events()
            .await
            .iter()
            .filter(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND))
            .count();
        assert_eq!(
            gw_after,
            gw_before + 1,
            "server must publish one new gift-wrap (kind {}) as the response",
            GIFT_WRAP_KIND
        );
    }
}

// ── 26. send_response is one-shot under concurrency ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_response_is_one_shot_under_concurrency() {
    let (client_pool, server_pool_raw) = MockRelayPool::create_pair();
    let server_pubkey = server_pool_raw.mock_public_key();
    let server_pool = Arc::new(server_pool_raw);

    // Delay publish so both concurrent responders have a chance to race.
    // Correct behavior is still one-shot: exactly one send_response succeeds.
    let delayed_server_pool: Arc<dyn RelayPoolTrait> = Arc::new(TestRelayPool::with_publish_delay(
        Arc::clone(&server_pool),
        Duration::from_millis(25),
    ));

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        delayed_server_pool,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("one-shot-req"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive request")
        .expect("server channel closed");

    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("placeholder"),
        result: serde_json::json!({ "one_shot": "ok" }),
    });

    let event_id = incoming.event_id.clone();
    let f1 = server.send_response(&event_id, response.clone());
    let f2 = server.send_response(&event_id, response);
    let (r1, r2) = tokio::join!(f1, f2);

    assert_ne!(
        r1.is_ok(),
        r2.is_ok(),
        "exactly one concurrent send_response call must succeed"
    );

    let msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to receive response")
        .expect("client channel closed");
    assert!(msg.is_response(), "client must receive one response");
    assert_eq!(
        msg.id(),
        Some(&serde_json::json!("one-shot-req")),
        "server must restore original request id in response"
    );

    let second = tokio::time::timeout(Duration::from_millis(200), client_rx.recv()).await;
    assert!(
        second.is_err(),
        "client must not receive duplicate response"
    );

    let events = server_pool.stored_events().await;
    let response_events = events
        .iter()
        .filter(|e| e.pubkey == server_pubkey && e.content.contains("\"one_shot\":\"ok\""))
        .count();
    assert_eq!(
        response_events, 1,
        "only one response event must be published"
    );
}

// ── 27. send_response publish failure allows retry ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_response_publish_failure_allows_one_successful_retry() {
    let (client_pool, server_pool_raw) = MockRelayPool::create_pair();
    let server_pubkey = server_pool_raw.mock_public_key();
    let server_pool = Arc::new(server_pool_raw);
    let failing_server_pool = Arc::new(TestRelayPool::with_publish_failures(
        Arc::clone(&server_pool),
        1,
    ));

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&failing_server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("retry-once"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server request")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/list"));

    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("placeholder"),
        result: serde_json::json!({ "tools": [] }),
    });

    let stored_before_failure = server_pool.stored_events().await.len();
    server
        .send_response(&incoming.event_id, response.clone())
        .await
        .expect_err("first response publish must fail");

    assert_eq!(
        failing_server_pool.publish_attempts(),
        1,
        "failed response should attempt exactly one publish"
    );
    assert_eq!(
        server_pool.stored_events().await.len(),
        stored_before_failure,
        "failed publish must not store a response event"
    );

    server
        .send_response(&incoming.event_id, response.clone())
        .await
        .expect("retry must still find the route and publish");

    let client_msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for retried response")
        .expect("client channel closed");
    assert!(client_msg.is_response());
    assert_eq!(client_msg.id(), Some(&serde_json::json!("retry-once")));
    assert_eq!(
        failing_server_pool.publish_attempts(),
        2,
        "retry should perform the second and final publish"
    );
    assert_eq!(
        server_pool.stored_events().await.len(),
        stored_before_failure + 1,
        "successful retry must publish exactly one response event"
    );

    server
        .send_response(&incoming.event_id, response)
        .await
        .expect_err("route must be consumed after the successful retry");
    assert_eq!(
        failing_server_pool.publish_attempts(),
        2,
        "consumed route should fail before another publish attempt"
    );
    assert_eq!(
        server_pool.stored_events().await.len(),
        stored_before_failure + 1,
        "post-success retry must not publish another response"
    );

    let second_delivery = tokio::time::timeout(Duration::from_millis(50), client_rx.recv()).await;
    assert!(
        second_delivery.is_err(),
        "client must receive the retried response exactly once"
    );
}
