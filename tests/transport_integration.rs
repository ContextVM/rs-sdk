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
use contextvm_sdk::core::constants::tags;
use contextvm_sdk::core::constants::{
    mcp_protocol_version, CTXVM_MESSAGES_KIND, EPHEMERAL_GIFT_WRAP_KIND, GIFT_WRAP_KIND,
    MAX_MESSAGE_SIZE, PROMPTS_LIST_KIND, RESOURCES_LIST_KIND, RESOURCETEMPLATES_LIST_KIND,
    SERVER_ANNOUNCEMENT_KIND, TOOLS_LIST_KIND,
};
use contextvm_sdk::core::types::{EncryptionMode, GiftWrapMode};
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::base::BaseTransport;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::oversized_transfer::{
    build_oversized_frames, OversizedFrame, OversizedSenderOptions, OversizedTransferConfig,
};
use contextvm_sdk::transport::server::{
    IncomingRequest, NostrServerTransport, NostrServerTransportConfig,
};
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

    async fn publish_to(
        &self,
        urls: &[String],
        builder: EventBuilder,
    ) -> contextvm_sdk::Result<EventId> {
        self.inner.publish_to(urls, builder).await
    }

    async fn fetch_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
    ) -> contextvm_sdk::Result<Vec<Event>> {
        self.inner.fetch_events(filters, timeout).await
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_server_info(ServerInfo::default().with_name("Phase3-Test-Server".to_string()))
            .with_announced_server(true),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Optional),
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
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_allowed_public_keys(vec![allowed_keys.public_key().to_hex()]),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Required),
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
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Required),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Required),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Required),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Required),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Optional),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Required),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Optional),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Optional),
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
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Required)
            .with_server_info(ServerInfo::default().with_name("Encrypted-Server".to_string()))
            .with_announced_server(true),
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
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_server_info(
                ServerInfo::default()
                    .with_name("Meta-Server".to_string())
                    .with_about("A test server".to_string())
                    .with_website("https://example.com".to_string())
                    .with_picture("https://example.com/pic.png".to_string()),
            )
            .with_announced_server(true),
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
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_server_info(ServerInfo::default().with_name("Tools-Server".to_string()))
            .with_announced_server(true),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(s_pool),
    )
    .await
    .expect("create server transport");

    let mut srv_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pk.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_allowed_public_keys(vec![client_pubkey.to_hex()]),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_allowed_public_keys(vec![allowed_keys.public_key().to_hex()])
            .with_excluded_capabilities(vec![CapabilityExclusion {
                method: "tools/list".to_string(),
                name: None,
            }]),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
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

// ── 23. Delete announcements uses e tags referencing published events ─────────

#[tokio::test]
async fn delete_announcements_uses_e_tags_for_published_events() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_server_info(ServerInfo::default().with_name("KTag-Server".to_string()))
            .with_announced_server(true),
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

    // Find the kind 11316 announcement that was published by announce()
    let announcement = events
        .iter()
        .find(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND))
        .expect("should have a kind 11316 announcement event");
    let announcement_id = announcement.id;

    // Only 1 deletion event expected: only kind 11316 was announced
    let kind5_events: Vec<_> = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(5))
        .collect();
    assert_eq!(
        kind5_events.len(),
        1,
        "only one kind was announced so only one deletion event expected"
    );

    let del = &kind5_events[0];

    // Deletion uses ["e", event_id] tags (not ["k", kind])
    let tags: Vec<Vec<String>> = del.tags.iter().map(|t| (*t).clone().to_vec()).collect();
    assert!(!tags.is_empty(), "deletion event should have tags");
    for tag in &tags {
        assert_eq!(tag[0], "e", "deletion tag should be 'e', not 'k'");
    }

    // The e tag must reference the announced event's ID
    let ann_id_hex = announcement_id.to_hex();
    assert!(
        tags.iter()
            .any(|t| t.get(1).map(|s| s.as_str()) == Some(ann_id_hex.as_str())),
        "deletion should reference the published announcement event ID"
    );

    // Content is the reason string
    assert_eq!(del.content, "shutting down");
}

// ── 24. Encryption Disabled server rejects gift-wrap ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encryption_disabled_server_rejects_gift_wrap() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Server has encryption disabled — must reject gift-wrap events.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
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
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Required),
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
            NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Optional),
            Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("create server transport");

        let mut client = NostrClientTransport::with_relay_pool(
            NostrClientTransportConfig::default()
                .with_relay_urls(vec!["wss://mock.relay".to_string()])
                .with_server_pubkey(server_pubkey.to_hex())
                .with_encryption_mode(EncryptionMode::Disabled),
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
            NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Optional),
            Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("create server transport");

        let mut client = NostrClientTransport::with_relay_pool(
            NostrClientTransportConfig::default()
                .with_relay_urls(vec!["wss://mock.relay".to_string()])
                .with_server_pubkey(server_pubkey.to_hex())
                .with_encryption_mode(EncryptionMode::Required),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        delayed_server_pool,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        Arc::clone(&failing_server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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

// ── 28. Announced server sends unauthorized error response ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announced_server_sends_unauthorized_error_response() {
    let allowed_keys = Keys::generate(); // a DIFFERENT pubkey — client is NOT in the allowlist
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Announced server with an allowlist that does NOT include the client.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_announced_server(true)
            .with_allowed_public_keys(vec![allowed_keys.public_key().to_hex()]),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Send a non-initialize request from the unauthorized client.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(42),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // The server handler must NOT receive the request (it's unauthorized).
    let server_forward = tokio::time::timeout(Duration::from_millis(300), server_rx.recv()).await;
    assert!(
        server_forward.is_err(),
        "unauthorized request must not reach the server handler"
    );

    // The client MUST receive a -32000 Unauthorized error response.
    let error_msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for unauthorized error response")
        .expect("client channel closed");

    match error_msg {
        JsonRpcMessage::ErrorResponse(err) => {
            assert_eq!(err.error.code, -32000, "error code must be -32000");
            assert_eq!(
                err.error.message, "Unauthorized",
                "error message must be 'Unauthorized'"
            );
        }
        other => panic!(
            "expected ErrorResponse, got: {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

// ── 29. Private server silently drops unauthorized request ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_server_silently_drops_unauthorized_request() {
    let allowed_keys = Keys::generate();
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Private server (is_announced_server defaults to false).
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_allowed_public_keys(vec![allowed_keys.public_key().to_hex()]),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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
        id: serde_json::json!(99),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // Server handler must not receive it.
    let server_forward = tokio::time::timeout(Duration::from_millis(300), server_rx.recv()).await;
    assert!(
        server_forward.is_err(),
        "unauthorized request must not reach the server handler"
    );

    // Client must NOT receive any error response (private server silently drops).
    let client_response = tokio::time::timeout(Duration::from_millis(300), client_rx.recv()).await;
    assert!(
        client_response.is_err(),
        "private server must silently drop unauthorized requests without sending an error"
    );
}

// ── 30. Announced server does not error on unauthorized notification ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announced_server_does_not_error_on_unauthorized_notification() {
    let allowed_keys = Keys::generate();
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_announced_server(true)
            .with_allowed_public_keys(vec![allowed_keys.public_key().to_hex()]),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Send a notification (not a request) from the unauthorized client.
    let notification = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/progress".to_string(),
        params: None,
    });
    client.send(&notification).await.expect("send notification");

    // Server handler must not receive the notification.
    let server_forward = tokio::time::timeout(Duration::from_millis(300), server_rx.recv()).await;
    assert!(
        server_forward.is_err(),
        "unauthorized notification must not reach the server handler"
    );

    // Client must NOT receive an error (notifications never get error replies).
    let client_response = tokio::time::timeout(Duration::from_millis(300), client_rx.recv()).await;
    assert!(
        client_response.is_err(),
        "announced server must not send error response for unauthorized notifications"
    );
}

// ── 31. First response includes discovery tags (upstream CEP-19) ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_response_includes_discovery_tags() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let s_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional)
            .with_server_info(ServerInfo::default().with_name("Disco-Server".to_string()))
            .with_announced_server(true),
        Arc::clone(&s_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
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

    // Send first request (triggers first response with common tags)
    let request1 = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("req-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request1).await.expect("send request 1");

    let incoming1 = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");

    let response1 = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("req-1"),
        result: serde_json::json!({ "tools": [] }),
    });
    server
        .send_response(&incoming1.event_id, response1)
        .await
        .expect("send response 1");

    // Send second request (should NOT include common tags)
    let request2 = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("req-2"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request2).await.expect("send request 2");

    let incoming2 = loop {
        let msg = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        if msg.message.is_request() && msg.message.id() == Some(&serde_json::json!("req-2")) {
            break msg;
        }
    };

    let response2 = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("req-2"),
        result: serde_json::json!({ "tools": [] }),
    });
    server
        .send_response(&incoming2.event_id, response2)
        .await
        .expect("send response 2");

    let events = s_pool.stored_events().await;
    let responses: Vec<_> = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .cloned()
        .collect();

    let resp1 = responses
        .iter()
        .find(|e| e.content.contains("req-1") && e.content.contains("result"))
        .expect("resp1 missing");
    let resp2 = responses
        .iter()
        .find(|e| e.content.contains("req-2") && e.content.contains("result"))
        .expect("resp2 missing");

    let name1 = contextvm_sdk::core::serializers::get_tag_value(&resp1.tags, "name");
    let enc1 = resp1
        .tags
        .iter()
        .any(|t| t.clone().to_vec().first().map(|s| s.as_str()) == Some("support_encryption"));

    let name2 = contextvm_sdk::core::serializers::get_tag_value(&resp2.tags, "name");
    let enc2 = resp2
        .tags
        .iter()
        .any(|t| t.clone().to_vec().first().map(|s| s.as_str()) == Some("support_encryption"));

    assert_eq!(name1.as_deref(), Some("Disco-Server"));
    assert!(enc1);

    assert_eq!(name2, None);
    assert!(!enc2);
}

// ── 32. Notification mirror selection wrt CEP 19 ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_mirror_selection_wrt_cep_19() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let s_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional),
        Arc::clone(&s_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Ephemeral),
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

    let request1 = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("req-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request1).await.expect("send request 1");

    let incoming1 = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");

    let notification = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/progress".to_string(),
        params: None,
    });
    server
        .send_notification(
            &incoming1.client_pubkey,
            &notification,
            Some(&incoming1.event_id),
        )
        .await
        .expect("send notification");

    let events = s_pool.stored_events().await;
    let ephemeral_wraps: Vec<_> = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND))
        .cloned()
        .collect();

    assert!(
        ephemeral_wraps.len() >= 2,
        "Expected ephemeral wraps for both request and notification"
    );
}

// ── CEP-35: Server-side discovery tag emission & capability learning ─────────

fn event_tag_vecs(event: &Event) -> Vec<Vec<String>> {
    event.tags.iter().map(|t| t.clone().to_vec()).collect()
}

fn has_tag_name(event: &Event, name: &str) -> bool {
    event_tag_vecs(event)
        .iter()
        .any(|v| v.first().map(|s| s.as_str()) == Some(name))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_response_includes_encryption_tags_when_enabled() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool_arc = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional),
        Arc::clone(&server_pool_arc) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_pool),
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    let mut client_rx = client.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();
    server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({}),
            }),
        )
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .unwrap();

    let events = server_pool_arc.stored_events().await;
    let response_event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND) && has_tag_name(e, "e"))
        .expect("response event must exist");

    assert!(
        has_tag_name(response_event, tags::SUPPORT_ENCRYPTION),
        "first response must include support_encryption when mode != Disabled"
    );
    assert!(
        has_tag_name(response_event, tags::SUPPORT_ENCRYPTION_EPHEMERAL),
        "first response must include support_encryption_ephemeral when GiftWrapMode != Persistent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_response_excludes_ephemeral_tag_when_persistent() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool_arc = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Persistent),
        Arc::clone(&server_pool_arc) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_pool),
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    let mut client_rx = client.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();
    server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({}),
            }),
        )
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .unwrap();

    let events = server_pool_arc.stored_events().await;
    let response_event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND) && has_tag_name(e, "e"))
        .unwrap();

    assert!(
        has_tag_name(response_event, tags::SUPPORT_ENCRYPTION),
        "support_encryption must be present"
    );
    assert!(
        !has_tag_name(response_event, tags::SUPPORT_ENCRYPTION_EPHEMERAL),
        "support_encryption_ephemeral must NOT be present when GiftWrapMode is Persistent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_learns_capabilities_from_client_request() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(server_pool),
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_pool),
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(incoming.message.method(), Some("initialize"));
    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(2),
            method: "tools/list".to_string(),
            params: None,
        }))
        .await
        .unwrap();
    let incoming2 = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(incoming2.message.method(), Some("tools/list"));
    assert_eq!(incoming.client_pubkey, incoming2.client_pubkey);
}

/// CEP-22: with oversized transfer enabled on the server, the server learns the
/// client's advertised `support_oversized_transfer` flag as a request flows
/// through the real `event_loop` gate. Exercises the production gate end-to-end
/// (not the inline truth-table unit test).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_gate_allows_oversized_when_enabled() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_enabled(true),
        as_pool(server_pool),
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_enabled(true),
        as_pool(client_pool),
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();

    // The session-state update precedes the IncomingRequest dispatch, so the
    // learned flag is committed by the time we receive the request.
    let snap = server
        .session_snapshot(&incoming.client_pubkey)
        .await
        .expect("server must have a session for the client");
    assert!(
        snap.supports_oversized_transfer,
        "server with oversized enabled must learn the client's advertised support"
    );
}

/// CEP-22: with oversized transfer disabled on the server, the gate drops the
/// client's advertised `support_oversized_transfer` flag — the session never
/// records support even though the client advertised it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_gate_blocks_oversized_when_disabled() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_enabled(false),
        as_pool(server_pool),
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_enabled(true),
        as_pool(client_pool),
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let snap = server
        .session_snapshot(&incoming.client_pubkey)
        .await
        .expect("server must have a session for the client");
    assert!(
        !snap.supports_oversized_transfer,
        "server with oversized disabled must not learn the client's advertised support"
    );
}

/// CEP-22: even with oversized transfer enabled, the server only learns support
/// the client actually advertised. A client that does not advertise the tag
/// leaves the session flag false. Pins the `&& discovered.supports_oversized_transfer`
/// operand of the gate — a regression dropping it would learn `true` regardless.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_gate_blocks_oversized_when_client_does_not_advertise() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    // Server is oversized-capable with a low threshold; the only thing that should
    // keep the (large) response un-fragmented is the client not advertising support.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(OversizedTransferConfig::enabled().with_threshold(2000)),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_enabled(false),
        as_pool(client_pool),
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    // Request carries a progressToken so the server's only remaining fragmentation
    // blocker is the missing client capability (not a missing token).
    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({ "_meta": { "progressToken": "tok-block" } })),
        }))
        .await
        .unwrap();

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let snap = server
        .session_snapshot(&incoming.client_pubkey)
        .await
        .expect("server must have a session for the client");
    assert!(
        !snap.supports_oversized_transfer,
        "server must not learn oversized support the client never advertised"
    );

    // Server replies with a payload well over the threshold. Because the client
    // never advertised support, the gate must block fragmentation: the response is
    // a single event, not start/chunk/end frames.
    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        result: serde_json::json!({ "blob": "Z".repeat(8000) }),
    });
    server
        .send_response(&incoming.event_id, response)
        .await
        .expect("server send response");

    let server_response_frames = server_pool
        .stored_events()
        .await
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND) && e.pubkey == server_pubkey)
        .count();
    assert_eq!(
        server_response_frames, 1,
        "gate off (client did not advertise) must send the response as exactly one event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_disabled_encryption_omits_encryption_tags() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool_arc = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_server_info(ServerInfo::default().with_name("NoEncrypt".to_string())),
        Arc::clone(&server_pool_arc) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_pool),
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    let mut client_rx = client.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();
    server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({}),
            }),
        )
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .unwrap();

    let events = server_pool_arc.stored_events().await;
    let response_event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND) && has_tag_name(e, "e"))
        .unwrap();

    assert!(has_tag_name(response_event, tags::NAME));
    assert!(
        !has_tag_name(response_event, tags::SUPPORT_ENCRYPTION),
        "encryption tags must be omitted when EncryptionMode is Disabled"
    );
    assert!(!has_tag_name(
        response_event,
        tags::SUPPORT_ENCRYPTION_EPHEMERAL
    ));
}

// ── CEP-35: Client-side discovery tag emission & capability learning ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_disabled_encryption_emits_no_discovery_tags() {
    // Disabled encryption: client must not emit cap tags. Positive case (Optional
    // mode emits tags) is covered by unit test client_capability_tags_encryption_optional.
    let pool = Arc::new(MockRelayPool::new());
    let server_keys = Keys::generate();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_keys.public_key().to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_gift_wrap_mode(GiftWrapMode::Optional),
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    client.start().await.unwrap();
    let_event_loops_start().await;

    // With Disabled encryption, no cap tags are emitted (correct per spec).
    // Verify the event is published with p tag but without cap tags.
    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let events = pool.stored_events().await;
    let client_event = events
        .iter()
        .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .expect("client must publish a request event");

    // p tag must be present (routing)
    assert!(has_tag_name(client_event, "p"));
    // No encryption tags when Disabled (the unit test covers the Optional case)
    assert!(
        !has_tag_name(client_event, tags::SUPPORT_ENCRYPTION),
        "Disabled client must not emit support_encryption"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_second_request_carries_no_discovery_tags() {
    // Second request must never carry discovery tags. One-shot flag behavior
    // is covered by unit test client_discovery_tags_sent_once.
    let pool = Arc::new(MockRelayPool::new());
    let server_keys = Keys::generate();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_keys.public_key().to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_gift_wrap_mode(GiftWrapMode::Optional),
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    client.start().await.unwrap();
    let_event_loops_start().await;

    // First request
    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    // Second request
    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(2),
            method: "tools/list".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let events = pool.stored_events().await;
    let ctxvm_events: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .collect();
    assert!(ctxvm_events.len() >= 2);

    let second_event = ctxvm_events
        .iter()
        .find(|e| e.content.contains("tools/list"))
        .expect("second request event must exist");

    assert!(
        !has_tag_name(second_event, tags::SUPPORT_ENCRYPTION),
        "second request must NOT include discovery tags"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_learns_server_capabilities_from_first_response() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional)
            .with_server_info(ServerInfo::default().with_name("CapServer".to_string())),
        as_pool(server_pool),
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_pool),
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    let mut client_rx = client.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();

    server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({}),
            }),
        )
        .await
        .unwrap();

    let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .unwrap();

    // Client should have learned capabilities from server's first response
    let caps = client.discovered_server_capabilities();
    assert!(
        caps.supports_encryption,
        "client must learn support_encryption from server response tags"
    );
    assert!(
        caps.supports_ephemeral_encryption,
        "client must learn support_encryption_ephemeral from server response tags"
    );

    let baseline = client.get_server_initialize_event();
    assert!(baseline.is_some(), "baseline event must be set");
}

// ── CEP-35: OR-assign, baseline-freeze, and Optional emission ────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_or_assigns_capabilities_across_responses() {
    // Server with Persistent gift-wrap emits support_encryption but NOT
    // support_encryption_ephemeral on the first response.  A second event
    // carrying support_encryption_ephemeral must OR-assign into the client's
    // learned caps without downgrading the already-learned support_encryption.
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_keys = server_pool.mock_keys();

    let client_pool = Arc::new(client_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Persistent)
            .with_server_info(ServerInfo::default().with_name("PersistentServer".to_string())),
        as_pool(server_pool),
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        Arc::clone(&client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    let mut client_rx = client.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    // First roundtrip — server responds with support_encryption only.
    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();

    server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({}),
            }),
        )
        .await
        .unwrap();

    let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .unwrap();

    let caps_after_first = client.discovered_server_capabilities();
    assert!(
        caps_after_first.supports_encryption,
        "first response must teach support_encryption"
    );
    assert!(
        !caps_after_first.supports_ephemeral_encryption,
        "Persistent server must NOT advertise ephemeral on first response"
    );

    // Inject a second plaintext event signed by the server, carrying
    // support_encryption_ephemeral (simulates a capability upgrade).
    let client_pubkey = client_pool.mock_public_key();
    let second_response = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress"
    });
    let inject_event = EventBuilder::new(
        Kind::Custom(CTXVM_MESSAGES_KIND),
        second_response.to_string(),
    )
    .tags(vec![
        Tag::public_key(client_pubkey),
        Tag::custom(
            TagKind::Custom(tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
            Vec::<String>::new(),
        ),
    ])
    .sign_with_keys(&server_keys)
    .unwrap();

    client_pool.publish_event(&inject_event).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let caps_after_second = client.discovered_server_capabilities();
    assert!(
        caps_after_second.supports_encryption,
        "support_encryption must survive OR-assign (not downgraded)"
    );
    assert!(
        caps_after_second.supports_ephemeral_encryption,
        "support_encryption_ephemeral must be OR-assigned from second event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_baseline_event_not_replaced_by_later_responses() {
    // The first inbound event carrying discovery tags becomes the baseline.
    // Later events with different tags must NOT replace it.
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_keys = server_pool.mock_keys();

    let client_pool = Arc::new(client_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional)
            .with_server_info(ServerInfo::default().with_name("BaselineServer".to_string())),
        as_pool(server_pool),
    )
    .await
    .unwrap();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        Arc::clone(&client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    let mut server_rx = server.take_message_receiver().unwrap();
    let mut client_rx = client.take_message_receiver().unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let_event_loops_start().await;

    // First roundtrip — establishes baseline.
    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .unwrap()
        .unwrap();

    server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                result: serde_json::json!({}),
            }),
        )
        .await
        .unwrap();

    let _ = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .unwrap();

    let baseline = client.get_server_initialize_event();
    assert!(
        baseline.is_some(),
        "baseline must be set after first response"
    );
    let baseline_id = baseline.unwrap().id;

    // Inject a second event with different discovery tags.
    let client_pubkey = client_pool.mock_public_key();
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress"
    });
    let inject_event =
        EventBuilder::new(Kind::Custom(CTXVM_MESSAGES_KIND), notification.to_string())
            .tags(vec![
                Tag::public_key(client_pubkey),
                Tag::custom(
                    TagKind::Custom(tags::SUPPORT_ENCRYPTION.into()),
                    Vec::<String>::new(),
                ),
            ])
            .sign_with_keys(&server_keys)
            .unwrap();

    client_pool.publish_event(&inject_event).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let baseline_after = client.get_server_initialize_event();
    assert_eq!(
        baseline_after.unwrap().id,
        baseline_id,
        "baseline event must NOT be replaced by later events"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_optional_encryption_emits_discovery_tags() {
    // Client with Optional encryption must include discovery tags in the
    // inner signed event.  We decrypt the published gift wrap to verify.
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_keys = server_pool.mock_keys();

    let client_pool = Arc::new(client_pool);

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional),
        Arc::clone(&client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .unwrap();

    client.start().await.unwrap();
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: None,
        }))
        .await
        .unwrap();

    let events = client_pool.stored_events().await;
    let gift_wrap = events
        .iter()
        .find(|e| {
            e.kind == Kind::Custom(GIFT_WRAP_KIND)
                || e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)
        })
        .expect("Optional encryption must produce a gift-wrapped event");

    // Decrypt using the server's keys (the recipient).
    let signer: Arc<dyn NostrSigner> = Arc::new(server_keys);
    let decrypted_json =
        contextvm_sdk::encryption::decrypt_gift_wrap_single_layer(&signer, gift_wrap)
            .await
            .expect("gift wrap must be decryptable with server keys");

    let inner: Event =
        serde_json::from_str(&decrypted_json).expect("decrypted content must be a valid Event");

    assert!(
        has_tag_name(&inner, tags::SUPPORT_ENCRYPTION),
        "inner event must carry support_encryption tag"
    );
    assert!(
        has_tag_name(&inner, tags::SUPPORT_ENCRYPTION_EPHEMERAL),
        "inner event must carry support_encryption_ephemeral tag (Optional gift-wrap mode)"
    );
}
// ── Multi-client support ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_client_concurrent_requests_both_get_responses() {
    // Two different clients send requests to the same server; both must get
    // their own response (the single-peer barrier is removed).
    let mut pools = MockRelayPool::create_linked_group(3);
    let server_pool = pools.remove(0);
    let client_b_pool = pools.remove(1);
    let client_a_pool = pools.remove(0);
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client_a = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_a_pool),
    )
    .await
    .expect("create client A");

    let mut client_b = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_b_pool),
    )
    .await
    .expect("create client B");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_a_rx = client_a
        .take_message_receiver()
        .expect("client A message receiver");
    let mut client_b_rx = client_b
        .take_message_receiver()
        .expect("client B message receiver");

    server.start().await.expect("server start");
    client_a.start().await.expect("client A start");
    client_b.start().await.expect("client B start");
    let_event_loops_start().await;

    // Client A sends a request.
    let req_a = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: "tools/list".to_string(),
        params: None,
    });
    client_a.send(&req_a).await.expect("client A send");

    // Client B sends a request.
    let req_b = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(2),
        method: "tools/list".to_string(),
        params: None,
    });
    client_b.send(&req_b).await.expect("client B send");

    // Server receives both requests (order may vary).
    let incoming_1 = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout rx 1")
        .expect("rx closed 1");
    let incoming_2 = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout rx 2")
        .expect("rx closed 2");

    // Send responses to both.
    let resp_1 = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: incoming_1.message.id().unwrap().clone(),
        result: serde_json::json!({"tools": []}),
    });
    server
        .send_response(&incoming_1.event_id, resp_1)
        .await
        .expect("server respond to 1");

    let resp_2 = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: incoming_2.message.id().unwrap().clone(),
        result: serde_json::json!({"tools": []}),
    });
    server
        .send_response(&incoming_2.event_id, resp_2)
        .await
        .expect("server respond to 2");

    // Both clients must receive their respective response.
    let resp_a = tokio::time::timeout(Duration::from_millis(500), client_a_rx.recv())
        .await
        .expect("timeout client A response")
        .expect("client A channel closed");
    let resp_b = tokio::time::timeout(Duration::from_millis(500), client_b_rx.recv())
        .await
        .expect("timeout client B response")
        .expect("client B channel closed");

    assert!(
        matches!(resp_a, JsonRpcMessage::Response(_)),
        "client A must receive a response"
    );
    assert!(
        matches!(resp_b, JsonRpcMessage::Response(_)),
        "client B must receive a response"
    );
}

// ── Session store LRU tests ─────────────────────────────────────────────────

use contextvm_sdk::transport::server::SessionStore;
use contextvm_sdk::ServerEventRouteStore;

#[tokio::test]
async fn session_store_lru_eviction() {
    let store = SessionStore::with_capacity(3);
    let r = ServerEventRouteStore::new();
    store.get_or_create_session("a", false, &r).await;
    store.get_or_create_session("b", false, &r).await;
    store.get_or_create_session("c", false, &r).await;

    // 4th session evicts the oldest ("a")
    store.get_or_create_session("d", false, &r).await;

    assert!(
        store.get_session("a").await.is_none(),
        "oldest session must be evicted when capacity is exceeded"
    );
    assert!(store.get_session("b").await.is_some());
    assert!(store.get_session("c").await.is_some());
    assert!(store.get_session("d").await.is_some());
    assert_eq!(store.session_count().await, 3);
}

#[tokio::test]
async fn session_store_eviction_callback_fires() {
    let evicted_keys: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = evicted_keys.clone();
    let r = ServerEventRouteStore::new();

    let mut store = SessionStore::with_capacity(2);
    store.set_eviction_callback(std::sync::Arc::new(move |pubkey| {
        captured.lock().unwrap().push(pubkey);
    }));

    store.get_or_create_session("x", false, &r).await;
    store.get_or_create_session("y", false, &r).await;
    // Adding "z" evicts "x"
    store.get_or_create_session("z", false, &r).await;

    let keys = evicted_keys.lock().unwrap();
    assert_eq!(keys.len(), 1, "callback must fire exactly once");
    assert_eq!(keys[0], "x", "evicted key must be the oldest session");
}

// ── Event loop cancellation on close() ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_close_stops_event_loop() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut rx = client.take_message_receiver().expect("message receiver");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Close should cancel the event loop, causing the rx channel to close.
    client.close().await.expect("client close");

    // The receiver must resolve to None (closed) within a short timeout.
    let result = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        matches!(result, Ok(None)),
        "after close(), message receiver must yield None (channel closed)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_close_stops_event_loop() {
    let (_client_pool, server_pool) = MockRelayPool::create_pair();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut rx = server.take_message_receiver().expect("message receiver");
    server.start().await.expect("server start");
    let_event_loops_start().await;

    // Close should cancel both event loop and cleanup tasks.
    server.close().await.expect("server close");

    // The receiver must resolve to None (closed) within a short timeout.
    let result = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        matches!(result, Ok(None)),
        "after close(), message receiver must yield None (channel closed)"
    );
}

// ── CEP-22: oversized transfer end-to-end wiring ─────────────────────────────

/// C→S: a request whose serialized size exceeds the threshold is fragmented into
/// multiple kind-25910 frames and reassembled into exactly one IncomingRequest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_request_roundtrip_client_to_server() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(6000)
        .with_chunk_size(6000);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // A request carrying a progressToken and a payload well over the threshold.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("big-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "_meta": { "progressToken": "tok-req" },
            "blob": "A".repeat(10000),
        })),
    });
    client.send(&request).await.expect("send oversized request");

    // The wire must carry more than one kind-25910 frame (start + chunks + end).
    let frame_count = server_pool
        .stored_events()
        .await
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .count();
    assert!(
        frame_count > 1,
        "oversized request must publish multiple frames, got {frame_count}"
    );

    // Exactly one reassembled request arrives at the server.
    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("timeout waiting for reassembled request")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/call"));
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("big-1")));

    // No second message should surface.
    let second = tokio::time::timeout(Duration::from_millis(100), server_rx.recv()).await;
    assert!(second.is_err(), "only one reassembled request expected");
}

/// CEP-22 parity (TS T3.1): one-shot discovery-tag placement across an oversized
/// *request*'s frames. The client advertises `support_oversized_transfer` on the
/// `start` frame only; the continuation (`chunk`/`end`) frames carry the bare
/// recipient tags. Encryption is Disabled so the plaintext frame tags are directly
/// inspectable — and `support_oversized_transfer` is advertised independently of
/// encryption mode (unlike `support_encryption`; see `get_client_capability_tags`),
/// so Disabled is a valid mode for this assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_continuation_frames_carry_no_discovery_tags() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(6000)
        .with_chunk_size(6000);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // The client's first (and only) request is oversized, so the one-shot discovery
    // tags ride its `start` frame.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("disco-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "_meta": { "progressToken": "tok-disco" },
            "blob": "A".repeat(10000),
        })),
    });
    client.send(&request).await.expect("send oversized request");

    // Confirm the transfer reassembled (so every frame has landed in the store).
    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("timeout waiting for reassembled request")
        .expect("server channel closed");
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("disco-1")));

    // Classify every cvm-bearing frame in the store by its `cvm.frameType`. The
    // client authors start/chunk/end; the server's `accept` frame matches none of
    // the three and is naturally excluded.
    let events = server_pool.stored_events().await;
    let mut start_frames: Vec<&Event> = Vec::new();
    let mut chunk_frames: Vec<&Event> = Vec::new();
    let mut end_frames: Vec<&Event> = Vec::new();
    for e in events
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
    {
        let frame_type = serde_json::from_str::<serde_json::Value>(&e.content)
            .ok()
            .as_ref()
            .and_then(|v| v.get("params"))
            .and_then(|p| p.get("cvm"))
            .and_then(|c| c.get("frameType"))
            .and_then(|f| f.as_str())
            .map(str::to_string);
        match frame_type.as_deref() {
            Some("start") => start_frames.push(e),
            Some("chunk") => chunk_frames.push(e),
            Some("end") => end_frames.push(e),
            _ => {}
        }
    }

    assert_eq!(
        start_frames.len(),
        1,
        "expected exactly one start frame, got {}",
        start_frames.len()
    );
    assert!(
        !chunk_frames.is_empty(),
        "expected at least one chunk frame for a fragmented request"
    );
    assert_eq!(
        end_frames.len(),
        1,
        "expected exactly one end frame, got {}",
        end_frames.len()
    );

    // The start frame advertises oversized support…
    assert!(
        has_tag_name(start_frames[0], tags::SUPPORT_OVERSIZED_TRANSFER),
        "the start frame must carry support_oversized_transfer"
    );
    // …and every continuation frame (chunk/end) must NOT repeat it.
    for &frame in chunk_frames.iter().chain(end_frames.iter()) {
        assert!(
            !has_tag_name(frame, tags::SUPPORT_OVERSIZED_TRANSFER),
            "continuation frames must not carry support_oversized_transfer"
        );
    }
}

/// Receive the next non-notification client message, skipping the stripped
/// progress forwards that precede an oversized response (their shape is pinned
/// by `oversized_progress_token_restored` in `oversized_timeout_e2e.rs`).
async fn recv_skipping_progress_forwards(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
) -> JsonRpcMessage {
    loop {
        let msg = tokio::time::timeout(Duration::from_millis(1000), rx.recv())
            .await
            .expect("timeout waiting for client message")
            .expect("client channel closed");
        if !matches!(msg, JsonRpcMessage::Notification(_)) {
            return msg;
        }
    }
}

/// S→C: a large response is fragmented by the server and reassembled by the
/// client into a single response delivered on its receiver.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_response_roundtrip_server_to_client() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(6000)
        .with_chunk_size(6000);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    let mut client_rx = client.take_message_receiver().expect("client rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Small request (single event) but carrying a progressToken so the server may
    // fragment its response.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("rsp-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({ "_meta": { "progressToken": "tok-rsp" } })),
    });
    client.send(&request).await.expect("send request");

    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("timeout waiting for request")
        .expect("server channel closed");

    // Server replies with a payload well over the threshold.
    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("rsp-1"),
        result: serde_json::json!({ "blob": "B".repeat(10000) }),
    });
    server
        .send_response(&incoming.event_id, response)
        .await
        .expect("server send oversized response");

    // The client reassembles and delivers the full response (preceded by
    // stripped progress forwards, skipped here).
    let client_msg = recv_skipping_progress_forwards(&mut client_rx).await;
    assert!(client_msg.is_response());
    assert_eq!(client_msg.id(), Some(&serde_json::json!("rsp-1")));
    if let JsonRpcMessage::Response(r) = client_msg {
        assert_eq!(r.result["blob"], serde_json::json!("B".repeat(10000)));
    } else {
        panic!("expected a response");
    }

    // The start frame of the oversized response carried the server's discovery
    // tags, so the client must have learned the server supports oversized
    // transfer by the time the reassembled response is delivered.
    assert!(
        client
            .discovered_server_capabilities()
            .supports_oversized_transfer,
        "client must learn server oversized support from the first oversized response"
    );
}

// ── CEP-22: number-or-string progressToken plumbing ──────────────────────────

/// Collect the `params` of every cvm-bearing (oversized-transfer) frame stored
/// in `pool`.
async fn stored_oversized_frame_params(pool: &MockRelayPool) -> Vec<serde_json::Value> {
    pool.stored_events()
        .await
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.content).ok())
        .filter_map(|v| v.get("params").cloned())
        .filter(|p| p.get("cvm").is_some())
        .collect()
}

/// C→S: rmcp stamps *numeric* progressTokens; a request carrying one must
/// still fragment (string-only extraction returned `None` and silently fell
/// back to the single-event path). On the wire every frame carries the
/// stringified token; the reassembled request preserves the original number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_request_roundtrip_numeric_token() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(6000)
        .with_chunk_size(6000);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // The token is the JSON number 7 — the shape rmcp emits.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("big-num-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "_meta": { "progressToken": 7 },
            "blob": "A".repeat(10000),
        })),
    });
    client.send(&request).await.expect("send oversized request");

    // Fragmentation must have happened, and every frame (start/chunks/end plus
    // the server's accept echo) must address the transfer by the *stringified*
    // token — numbers never appear on the wire.
    let frame_params = stored_oversized_frame_params(&server_pool).await;
    assert!(
        frame_params.len() > 1,
        "numeric-token oversized request must publish multiple frames, got {}",
        frame_params.len()
    );
    for params in &frame_params {
        assert_eq!(
            params.get("progressToken"),
            Some(&serde_json::json!("7")),
            "frames must carry the stringified token, got {params}"
        );
    }

    // Exactly one reassembled request arrives, original numeric token intact.
    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("timeout waiting for reassembled request")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/call"));
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("big-num-1")));
    if let JsonRpcMessage::Request(r) = &incoming.message {
        assert_eq!(
            r.params.as_ref().unwrap()["_meta"]["progressToken"],
            serde_json::json!(7),
            "reassembly must reproduce the original numeric token"
        );
    } else {
        panic!("expected a request");
    }

    let second = tokio::time::timeout(Duration::from_millis(100), server_rx.recv()).await;
    assert!(second.is_err(), "only one reassembled request expected");
}

/// S→C: a request whose `_meta.progressToken` is numeric must still open
/// the server's response fragmentation gate (string-only extraction left
/// `route.progress_token = None`, so responses to rmcp clients never
/// fragmented). Response frames carry the stringified token on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_response_roundtrip_numeric_token() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(6000)
        .with_chunk_size(6000);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    let mut client_rx = client.take_message_receiver().expect("client rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Small request (single event) carrying the JSON number 7 as its token.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("rsp-num-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({ "_meta": { "progressToken": 7 } })),
    });
    client.send(&request).await.expect("send request");

    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("timeout waiting for request")
        .expect("server channel closed");

    // Server replies with a payload well over the threshold.
    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("rsp-num-1"),
        result: serde_json::json!({ "blob": "B".repeat(10000) }),
    });
    server
        .send_response(&incoming.event_id, response)
        .await
        .expect("server send oversized response");

    // The client reassembles and delivers the full response (preceded by
    // stripped progress forwards, skipped here).
    let client_msg = recv_skipping_progress_forwards(&mut client_rx).await;
    assert!(client_msg.is_response());
    assert_eq!(client_msg.id(), Some(&serde_json::json!("rsp-num-1")));
    if let JsonRpcMessage::Response(r) = client_msg {
        assert_eq!(r.result["blob"], serde_json::json!("B".repeat(10000)));
    } else {
        panic!("expected a response");
    }

    // The response was actually fragmented, addressed by the stringified token.
    let frame_params = stored_oversized_frame_params(&server_pool).await;
    assert!(
        frame_params.len() > 1,
        "numeric-token response must fragment, got {} frames",
        frame_params.len()
    );
    for params in &frame_params {
        assert_eq!(
            params.get("progressToken"),
            Some(&serde_json::json!("7")),
            "response frames must carry the stringified token, got {params}"
        );
    }
}

// ── CEP-22: remaining oversized-transfer tests ───────────────────────────────

/// Start an oversized-enabled server over `server_pool`, returning the live
/// transport (keep alive) and its request receiver.
async fn start_oversized_server(
    server_pool: Arc<MockRelayPool>,
    encryption_mode: EncryptionMode,
    oversized: OversizedTransferConfig,
) -> (
    NostrServerTransport,
    tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
) {
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(encryption_mode)
            .with_oversized_transfer(oversized),
        server_pool as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");
    let rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    (server, rx)
}

/// A plaintext `BaseTransport` over `client_pool` used to inject raw frames
/// (grey-box: bypasses `NostrClientTransport::send`).
fn greybox_client_base(client_pool: MockRelayPool) -> BaseTransport {
    BaseTransport {
        relay_pool: as_pool(client_pool),
        encryption_mode: EncryptionMode::Disabled,
        is_connected: true,
    }
}

/// Publish one frame as a plaintext kind-25910 event tagged to `server_pubkey`.
async fn publish_plain_frame(
    base: &BaseTransport,
    server_pubkey: &PublicKey,
    tags: &[Tag],
    frame: JsonRpcNotification,
) {
    let message = JsonRpcMessage::Notification(frame);
    base.send_mcp_message(
        &message,
        server_pubkey,
        CTXVM_MESSAGES_KIND,
        tags.to_vec(),
        Some(false),
        None,
    )
    .await
    .expect("publish frame");
}

/// A request whose serialized form carries `progressToken` and a payload of
/// `blob_len` bytes (used to force fragmentation).
fn oversized_request(id: &str, token: &str, blob_len: usize) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "_meta": { "progressToken": token },
            "blob": "Q".repeat(blob_len),
        })),
    })
}

/// Out-of-order delivery: chunks published in a permuted order (within the
/// out-of-order window) still reassemble into the original request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_out_of_order_delivery_reassembles() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(64)
        .with_chunk_size(32);
    let (_server, mut server_rx) = start_oversized_server(
        Arc::clone(&server_pool),
        EncryptionMode::Disabled,
        oversized,
    )
    .await;
    let_event_loops_start().await;

    let base = greybox_client_base(client_pool);
    let tags = BaseTransport::create_recipient_tags(&server_pubkey);

    let request = oversized_request("ooo-1", "tok-ooo", 300);
    let serialized = serde_json::to_string(&request).unwrap();
    let opts = OversizedSenderOptions::new("tok-ooo")
        .with_chunk_size(32)
        .with_accept_handshake(true);
    let frames = build_oversized_frames(&serialized, &opts).unwrap();
    let start = frames.start;
    let mut chunks = frames.chunks;
    let end = frames.end;
    assert!(
        chunks.len() > 2,
        "need several chunks to permute meaningfully"
    );

    // start first; then the base chunk (so the reserved-accept-slot guard is
    // satisfied); then the remaining chunks reversed; then end.
    publish_plain_frame(&base, &server_pubkey, &tags, start).await;
    let base_chunk = chunks.remove(0);
    publish_plain_frame(&base, &server_pubkey, &tags, base_chunk).await;
    for chunk in chunks.into_iter().rev() {
        publish_plain_frame(&base, &server_pubkey, &tags, chunk).await;
    }
    publish_plain_frame(&base, &server_pubkey, &tags, end).await;

    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("timeout waiting for reassembled request")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/call"));
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("ooo-1")));
}

/// Digest mismatch: corrupting one chunk's `cvm.data` fails reassembly, and no
/// partial message is ever surfaced (recv times out).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_digest_mismatch_surfaces_nothing() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(64)
        .with_chunk_size(32);
    let (_server, mut server_rx) = start_oversized_server(
        Arc::clone(&server_pool),
        EncryptionMode::Disabled,
        oversized,
    )
    .await;
    let_event_loops_start().await;

    let base = greybox_client_base(client_pool);
    let tags = BaseTransport::create_recipient_tags(&server_pubkey);

    let request = oversized_request("dig-1", "tok-dig", 300);
    let serialized = serde_json::to_string(&request).unwrap();
    let opts = OversizedSenderOptions::new("tok-dig")
        .with_chunk_size(32)
        .with_accept_handshake(true);
    let frames = build_oversized_frames(&serialized, &opts).unwrap();
    let start = frames.start;
    let mut chunks = frames.chunks;
    let end = frames.end;

    // Corrupt a middle chunk's payload so the reassembled bytes/digest mismatch.
    let mid = chunks.len() / 2;
    if let Some(params) = chunks[mid].params.as_mut() {
        params["cvm"]["data"] = serde_json::json!("corrupted-bytes-not-original");
    }

    publish_plain_frame(&base, &server_pubkey, &tags, start).await;
    for chunk in chunks {
        publish_plain_frame(&base, &server_pubkey, &tags, chunk).await;
    }
    publish_plain_frame(&base, &server_pubkey, &tags, end).await;

    let result = tokio::time::timeout(Duration::from_millis(400), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "digest mismatch must not surface any (partial) message"
    );
}

/// Abort: a `start` followed by an `abort` frame terminates the transfer; no
/// message surfaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_abort_surfaces_nothing() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(64)
        .with_chunk_size(32);
    let (_server, mut server_rx) = start_oversized_server(
        Arc::clone(&server_pool),
        EncryptionMode::Disabled,
        oversized,
    )
    .await;
    let_event_loops_start().await;

    let base = greybox_client_base(client_pool);
    let tags = BaseTransport::create_recipient_tags(&server_pubkey);

    let request = oversized_request("abt-1", "tok-abt", 300);
    let serialized = serde_json::to_string(&request).unwrap();
    let opts = OversizedSenderOptions::new("tok-abt")
        .with_chunk_size(32)
        .with_accept_handshake(true);
    let frames = build_oversized_frames(&serialized, &opts).unwrap();

    publish_plain_frame(&base, &server_pubkey, &tags, frames.start).await;
    let abort = OversizedFrame::Abort { reason: None }
        .into_progress_notification("tok-abt", 2, None)
        .expect("build abort frame");
    publish_plain_frame(&base, &server_pubkey, &tags, abort).await;

    let result = tokio::time::timeout(Duration::from_millis(400), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "an aborted transfer must not surface a message"
    );
}

/// Per-frame size bound: every published kind-25910 frame stays under the 1 MiB
/// single-message cap, even for a payload far larger than one frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_each_frame_under_max_message_size() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    // Default chunk size (48_000) keeps each frame well under MAX_MESSAGE_SIZE.
    let oversized = OversizedTransferConfig::enabled();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // ~200 KB payload → several ~48 KB frames.
    let request = oversized_request("sz-1", "tok-sz", 200_000);
    client.send(&request).await.expect("send oversized request");

    let incoming = tokio::time::timeout(Duration::from_millis(1500), server_rx.recv())
        .await
        .expect("timeout waiting for reassembled request")
        .expect("server channel closed");
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("sz-1")));

    let frames: Vec<_> = server_pool
        .stored_events()
        .await
        .into_iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .collect();
    assert!(frames.len() > 1, "payload must have been fragmented");
    for frame in &frames {
        assert!(
            frame.content.len() < MAX_MESSAGE_SIZE,
            "every frame must stay under MAX_MESSAGE_SIZE, got {}",
            frame.content.len()
        );
    }
}

/// A reassembled payload larger than the 1 MiB single-message cap is delivered
/// in full (the per-transfer cap defaults to 100 MiB).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_reassembled_over_one_mib_succeeds() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // ~1.2 MB payload, comfortably over MAX_MESSAGE_SIZE (1 MiB).
    let blob_len = 1_200_000;
    let request = oversized_request("big-mib", "tok-mib", blob_len);
    let serialized_len = serde_json::to_string(&request).unwrap().len();
    assert!(
        serialized_len > MAX_MESSAGE_SIZE,
        "request must exceed 1 MiB"
    );

    client.send(&request).await.expect("send >1MiB request");

    let incoming = tokio::time::timeout(Duration::from_millis(3000), server_rx.recv())
        .await
        .expect("timeout waiting for >1MiB reassembled request")
        .expect("server channel closed");
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("big-mib")));
    if let JsonRpcMessage::Request(req) = incoming.message {
        assert_eq!(
            req.params.unwrap()["blob"],
            serde_json::json!("Q".repeat(blob_len))
        );
    } else {
        panic!("expected a request");
    }
}

/// Gate off: with oversized transfer disabled on the client, an above-threshold
/// request is published as a single event (no fragmentation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_gate_off_sends_single_event() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    // Server is oversized-capable, but the client's gate is OFF.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(OversizedTransferConfig::enabled().with_threshold(256)),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            // Explicitly disabled gate (the default flipped to enabled);
            // threshold present only to prove it is above-threshold.
            .with_oversized_transfer(
                OversizedTransferConfig::default()
                    .with_enabled(false)
                    .with_threshold(256),
            ),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Above the 256-byte threshold, but below the 1 MiB single-event cap.
    let request = oversized_request("gate-off", "tok-gate", 2000);
    client.send(&request).await.expect("send request");

    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("timeout waiting for request")
        .expect("server channel closed");
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("gate-off")));

    let frame_count = server_pool
        .stored_events()
        .await
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .count();
    assert_eq!(
        frame_count, 1,
        "gate off must publish exactly one single event, got {frame_count}"
    );
}

/// Oversized request roundtrip under a given encryption mode.
async fn run_oversized_request_roundtrip_mode(encryption_mode: EncryptionMode) {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(6000)
        .with_chunk_size(6000);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(encryption_mode)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(encryption_mode)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = oversized_request("mode-1", "tok-mode", 10000);
    client.send(&request).await.expect("send oversized request");

    let incoming = tokio::time::timeout(Duration::from_millis(1500), server_rx.recv())
        .await
        .expect("timeout waiting for reassembled request")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/call"));
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("mode-1")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_request_roundtrip_encryption_disabled() {
    run_oversized_request_roundtrip_mode(EncryptionMode::Disabled).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_request_roundtrip_encryption_optional() {
    run_oversized_request_roundtrip_mode(EncryptionMode::Optional).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_request_roundtrip_encryption_required() {
    run_oversized_request_roundtrip_mode(EncryptionMode::Required).await;
}

// ── CEP-22 Gap #1: oversized decision measures the published event ────────────

/// Send `request` through a fresh client/server pair under `encryption_mode` with
/// the given oversized `threshold`, then return how many kind-25910 (plaintext
/// frames) and kind-1059 (gift-wrapped frames) events the client published. A
/// single non-oversized send is one event; a fragmented send is several.
async fn oversized_published_frame_counts(
    encryption_mode: EncryptionMode,
    threshold: usize,
    request: &JsonRpcMessage,
) -> (usize, usize) {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled().with_threshold(threshold);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(encryption_mode)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(encryption_mode)
            .with_oversized_transfer(oversized),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    client.send(request).await.expect("send request");

    // Wait for the request to surface (single event or fully reassembled transfer),
    // then settle briefly so every frame has landed in the shared store.
    let _ = tokio::time::timeout(Duration::from_millis(1500), server_rx.recv()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let events = server_pool.stored_events().await;
    let kind_25910 = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .count();
    let kind_1059 = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND))
        .count();
    (kind_25910, kind_1059)
}

/// Gap #1 regression: the oversized decision is made on the *published* Nostr
/// event size, not the raw payload. A request whose raw serialized length is below
/// the threshold is sent as a single event in plaintext, but the SAME payload must
/// be fragmented once gift-wrap encryption inflates the published event past the
/// threshold (base64 ×1.333 + NIP-44 padding). Mirrors TS
/// `measurePublishedMcpMessageSize`; the old raw-length check sent both as a single
/// (over-cap) event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_decision_accounts_for_encryption_inflation() {
    // Margins (measured): payload=5900, threshold=8000.
    //   raw serialized      ≈ 6013 bytes  (< threshold — old check fragmented neither)
    //   plaintext published  ≈ 6455 bytes  (< threshold → single event)
    //   encrypted published  ≈ 10065 bytes (> threshold → fragments)
    // The 5900 payload is chosen so gift-wrap inflation (base64 ×1.333 + NIP-44
    // padding) is what crosses the threshold; adjust it if the envelope changes.
    let threshold = 8000;
    let request = oversized_request("enc-infl", "tok-infl", 5900);

    // Precondition: the raw payload is below the threshold, so the *old* raw-length
    // decision would have sent a single event in BOTH modes.
    let raw_len = serde_json::to_string(&request)
        .expect("serialize request")
        .len();
    assert!(
        raw_len < threshold,
        "precondition: raw payload ({raw_len}) must be below the threshold ({threshold})"
    );

    // Plaintext: the published event stays under the threshold → exactly one event.
    let (plain_25910, _) =
        oversized_published_frame_counts(EncryptionMode::Disabled, threshold, &request).await;
    assert_eq!(
        plain_25910, 1,
        "a below-threshold plaintext request must publish exactly one kind-25910 event, got {plain_25910}"
    );

    // Encrypted: gift-wrap inflation pushes the published event over the threshold,
    // so the SAME payload is now fragmented into multiple gift-wrapped frames.
    let (_, enc_1059) =
        oversized_published_frame_counts(EncryptionMode::Required, threshold, &request).await;
    assert!(
        enc_1059 > 1,
        "the same request must fragment once gift-wrap inflates it past the threshold, got {enc_1059} gift-wrap frames"
    );
}

// ── CEP-22: server accept-frame emission shape ───────────────────────────────

/// Poll the shared event store for the server's emitted `accept` frame (a
/// plaintext kind-25910 notification authored by the server whose `cvm.frameType`
/// is `accept`). Panics if none appears within the window.
async fn poll_for_accept_frame(
    server_pool: &Arc<MockRelayPool>,
    server_pubkey: &PublicKey,
) -> JsonRpcNotification {
    for _ in 0..50 {
        for event in server_pool.stored_events().await {
            if event.kind != Kind::Custom(CTXVM_MESSAGES_KIND) || event.pubkey != *server_pubkey {
                continue;
            }
            if let Ok(notif) = serde_json::from_str::<JsonRpcNotification>(&event.content) {
                let is_accept = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("cvm"))
                    .and_then(|cvm| cvm.get("frameType"))
                    .and_then(|ft| ft.as_str())
                    == Some("accept");
                if is_accept {
                    return notif;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not emit an accept frame within the poll window");
}

/// When a client sends an oversized `start` before the server knows it supports
/// oversized transfer, the server emits an `accept` frame with `progress == 2`
/// and `cvm.frameType == "accept"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_emits_accept_frame_with_expected_shape() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(2000)
        .with_chunk_size(2000);
    let (_server, _server_rx) = start_oversized_server(
        Arc::clone(&server_pool),
        EncryptionMode::Disabled,
        oversized,
    )
    .await;
    let_event_loops_start().await;

    // Grey-box: inject only the `start` frame (handshake slot reserved) from a
    // client that has not advertised oversized support.
    let base = greybox_client_base(client_pool);
    let tags = BaseTransport::create_recipient_tags(&server_pubkey);
    let request = oversized_request("acc-1", "tok-acc", 300);
    let serialized = serde_json::to_string(&request).unwrap();
    let opts = OversizedSenderOptions::new("tok-acc")
        .with_chunk_size(64)
        .with_accept_handshake(true);
    let frames = build_oversized_frames(&serialized, &opts).unwrap();
    publish_plain_frame(&base, &server_pubkey, &tags, frames.start).await;

    let accept = poll_for_accept_frame(&server_pool, &server_pubkey).await;
    let params = accept.params.as_ref().expect("accept frame has params");
    assert_eq!(
        params["progress"].as_u64(),
        Some(2),
        "accept frame must be published at progress slot 2"
    );
    assert_eq!(
        params["cvm"]["frameType"].as_str(),
        Some("accept"),
        "frame must be an oversized-transfer accept"
    );
    assert_eq!(
        params["cvm"]["type"].as_str(),
        Some("oversized-transfer"),
        "accept frame must carry the oversized-transfer cvm type"
    );
    assert_eq!(
        params["progressToken"].as_str(),
        Some("tok-acc"),
        "accept frame must echo the transfer's progressToken"
    );
    assert_eq!(
        params["message"].as_str(),
        Some("oversized request accepted"),
        "accept frame must carry the TS-matching human-readable message"
    );
}
