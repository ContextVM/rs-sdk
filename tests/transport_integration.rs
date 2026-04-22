//! Integration tests — transport-level flows using MockRelayPool.
//!
//! Each test wires client and/or server transports to an in-memory mock relay
//! network so that the full event-loop logic (subscription, publish, routing,
//! encryption-mode enforcement, and authorization) is exercised without
//! connecting to real relays.

use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::core::constants::{
    mcp_protocol_version, GIFT_WRAP_KIND, SERVER_ANNOUNCEMENT_KIND,
};
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use contextvm_sdk::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RelayPoolTrait,
    ServerInfo,
};
use nostr_sdk::prelude::*;

fn as_pool(pool: MockRelayPool) -> Arc<dyn RelayPoolTrait> {
    Arc::new(pool)
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
