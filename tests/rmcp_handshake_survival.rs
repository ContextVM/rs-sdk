//! Survival of the multiplexed rmcp server service against pre-initialize inbound traffic.
//!
//! rmcp's pre-service handshake accepts only an `initialize` request as its first message. The
//! worker multiplexes every Nostr client through one rmcp service, so a single inbound message of
//! any other shape reaching that handshake first terminates the service, cancels the worker, and
//! closes the transport for every client. These tests drive the real worker over `MockRelayPool`
//! and assert the server keeps serving.
//!
//! Timing uses the real tokio clock throughout (bounded `timeout` / `sleep`, never paused time):
//! the transport's own session and cleanup paths run on `std::time::Instant`.
//!
//! The server runs on `NostrServerTransportConfig::default()` (empty allowlist, not announced)
//! except where a test names the option it changes; hostile events are published in plaintext from
//! a separate linked pool, which the default `EncryptionMode::Optional` accepts.

#![cfg(feature = "rmcp")]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contextvm_sdk::core::constants::{
    mcp_protocol_version, CTXVM_MESSAGES_KIND, SERVER_ANNOUNCEMENT_KIND, TOOLS_LIST_KIND,
};
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::base::BaseTransport;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{
    InboundContext, InboundMiddleware, Next, NostrServerTransport, NostrServerTransportConfig,
};
use contextvm_sdk::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, RelayPoolTrait,
};
use nostr_sdk::prelude::*;
use rmcp::model::{
    ClientCapabilities, ClientInfo, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{ClientHandler, ServerHandler, ServiceExt};

/// The sentinel id the worker's synthetic startup `initialize` carries. Its response must be
/// dropped before wire correlation, so this string must never appear in a published event.
const STATELESS_SENTINEL: &str = "contextvm-stateless-init";

// ── Handlers ──────────────────────────────────────────────────────────────

/// Minimal MCP server, declaring rmcp's latest protocol version. `list_tools` is rmcp's default
/// (an empty list): these tests assert that a request is answered, not what it answers with.
struct SurvivalServer;

impl ServerHandler for SurvivalServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.protocol_version = ProtocolVersion::LATEST;
        info.server_info = Implementation::new("handshake-survival-server", "0.1.0");
        info
    }
}

/// Minimal MCP client. The requested protocol version varies per test, so it is a field.
struct SurvivalClient {
    protocol_version: ProtocolVersion,
}

impl SurvivalClient {
    fn latest() -> Self {
        Self::requesting(ProtocolVersion::LATEST)
    }

    fn requesting(protocol_version: ProtocolVersion) -> Self {
        Self { protocol_version }
    }
}

impl ClientHandler for SurvivalClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("handshake-survival-client", "0.1.0"),
        )
        .with_protocol_version(self.protocol_version.clone())
    }
}

/// A middleware that forwards everything. Registering any middleware at all moves inbound
/// messages off the event loop's inline path and onto a detached per-message task, which is the
/// ordering this exercises.
struct ForwardAll;

#[async_trait]
impl InboundMiddleware for ForwardAll {
    async fn handle(&self, message: JsonRpcMessage, _ctx: &InboundContext, next: Next) -> bool {
        next.run(message).await
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn as_pool(pool: &Arc<MockRelayPool>) -> Arc<dyn RelayPoolTrait> {
    Arc::clone(pool) as Arc<dyn RelayPoolTrait>
}

/// Give the spawned worker time to run `start()` and register its relay subscription, so a
/// hostile event published next is delivered live rather than missed.
async fn let_subscriptions_settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

fn request(id: &str, method: &str) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: method.to_string(),
        params: Some(serde_json::json!({})),
    })
}

fn initialize_request(id: &str) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": { "name": "raw-wire-client", "version": "0.1.0" }
        })),
    })
}

fn initialized_notification() -> JsonRpcMessage {
    JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/initialized".to_string(),
        params: None,
    })
}

/// A plaintext ContextVM message event addressed to `server`, signed by `keys`.
fn wire_event(keys: &Keys, server: PublicKey, message: &JsonRpcMessage) -> Event {
    let tags = BaseTransport::create_recipient_tags(&server);
    contextvm_sdk::core::serializers::mcp_to_nostr_event(message, CTXVM_MESSAGES_KIND, tags)
        .expect("serialize wire message")
        .sign_with_keys(keys)
        .expect("sign wire message")
}

/// Publish one message onto the shared mock relay network from `keys`, which is not on the
/// server's allowlist (the default allowlist is empty, so nothing is).
async fn publish_from(
    pool: &Arc<MockRelayPool>,
    keys: &Keys,
    server: PublicKey,
    message: &JsonRpcMessage,
) {
    pool.publish_event(&wire_event(keys, server, message))
        .await
        .expect("publish wire event");
}

async fn server_transport(pool: &Arc<MockRelayPool>) -> NostrServerTransport {
    NostrServerTransport::with_relay_pool(NostrServerTransportConfig::default(), as_pool(pool))
        .await
        .expect("create server transport")
}

/// Serve `handler` over `transport` on a spawned task. The returned receiver fires once `serve()`
/// has resolved `Ok`, i.e. the rmcp handshake completed and the service is running.
fn spawn_server(
    handler: SurvivalServer,
    transport: NostrServerTransport,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        match handler.serve(transport).await {
            Ok(running) => {
                let _ = ready_tx.send(());
                let _ = running.waiting().await;
            }
            Err(error) => {
                eprintln!("server serve() failed: {error}");
            }
        }
    });
    (handle, ready_rx)
}

/// A client transport talking plaintext to `server`, over its own pool in the shared network.
async fn client_transport(pool: &Arc<MockRelayPool>, server: PublicKey) -> NostrClientTransport {
    NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_server_pubkey(server.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_relay_urls(vec!["wss://mock.relay".to_string()]),
        as_pool(pool),
    )
    .await
    .expect("create client transport")
}

/// The core survival assertion: a fresh, well-behaved client completes the MCP handshake and gets
/// a `tools/list` answered. On a dead server both time out.
async fn assert_client_can_serve_and_list(pool: &Arc<MockRelayPool>, server: PublicKey) {
    let transport = client_transport(pool, server).await;
    let client = tokio::time::timeout(
        Duration::from_secs(5),
        SurvivalClient::latest().serve(transport),
    )
    .await
    .expect("client serve() timed out: the server transport is dead")
    .expect("client serve() failed");

    tokio::time::timeout(Duration::from_secs(5), client.list_all_tools())
        .await
        .expect("list_all_tools timed out: the server transport is dead")
        .expect("list_all_tools failed");

    client.cancel().await.expect("client cancel");
}

/// Publish `hostile` from a stranger's key as the first message the server ever sees, then
/// require a fresh, well-behaved client to still connect and get a `tools/list` answered.
async fn assert_survives_hostile_first_message(hostile: JsonRpcMessage) {
    let mut pools = MockRelayPool::create_linked_group(3);
    let server_pool = Arc::new(pools.remove(0));
    let client_pool = Arc::new(pools.remove(0));
    let stranger_pool = Arc::new(pools.remove(0));
    let server_pubkey = server_pool.mock_public_key();

    let transport = server_transport(&server_pool).await;
    let (handle, _ready) = spawn_server(SurvivalServer, transport);
    let_subscriptions_settle().await;

    publish_from(&stranger_pool, &Keys::generate(), server_pubkey, &hostile).await;
    // Let the hostile event reach the handshake before the honest client's initialize does.
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_client_can_serve_and_list(&client_pool, server_pubkey).await;

    handle.abort();
}

/// Poll the shared store until an event authored by `author` contains `needle`.
async fn wait_for_event_containing(
    pool: &Arc<MockRelayPool>,
    author: PublicKey,
    needle: &str,
    within: Duration,
) -> Event {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(event) = pool
            .stored_events()
            .await
            .into_iter()
            .find(|e| e.pubkey == author && e.content.contains(needle))
        {
            return event;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("no event from {author} containing {needle} within {within:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll the shared store until an event of `kind` exists.
async fn wait_for_kind(pool: &Arc<MockRelayPool>, kind: u16, within: Duration) -> Event {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(event) = pool
            .stored_events()
            .await
            .into_iter()
            .find(|e| e.kind == Kind::Custom(kind))
        {
            return event;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("no event of kind {kind} within {within:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The synthetic startup handshake is worker-local: nothing carrying its sentinel id may be
/// published.
async fn assert_sentinel_never_hit_the_wire(pool: &Arc<MockRelayPool>) {
    let leaked: Vec<String> = pool
        .stored_events()
        .await
        .into_iter()
        .filter(|e| e.content.contains(STATELESS_SENTINEL))
        .map(|e| e.content)
        .collect();
    assert!(
        leaked.is_empty(),
        "synthetic handshake messages must never reach the wire, found: {leaked:?}"
    );
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// The worker satisfies the rmcp handshake itself, so `serve()` resolves with no client traffic at
/// all. Nothing else can satisfy it: without the worker's startup send, `serve()` stays pending
/// until some client happens to send an `initialize`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_service_ready_without_client_traffic() {
    let server_pool = Arc::new(MockRelayPool::new());
    let transport = server_transport(&server_pool).await;

    let (handle, ready) = spawn_server(SurvivalServer, transport);

    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .expect("serve() must resolve without any client traffic")
        .expect("the server task ended before serve() resolved");

    assert_sentinel_never_hit_the_wire(&server_pool).await;

    handle.abort();
}

/// One `notifications/initialized` from an unauthenticated stranger, before any request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stranger_notification_before_init_survives() {
    assert_survives_hostile_first_message(initialized_notification()).await;
}

/// A response shape (`{"id":123,"result":{}}`) as the first inbound message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn response_shape_before_init_survives() {
    assert_survives_hostile_first_message(JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(123),
        result: serde_json::json!({}),
    }))
    .await;
}

/// An error-response shape as the first inbound message (the same handshake arm as a response).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_response_before_init_survives() {
    assert_survives_hostile_first_message(JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        error: JsonRpcError {
            code: -32000,
            message: "x".to_string(),
            data: None,
        },
    }))
    .await;
}

/// No attacker: one honest client whose `notifications/initialized` overtakes its own
/// `initialize`, which Nostr permits since it does not order delivery. Driven on the raw wire so
/// the test picks the order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reordered_initialized_before_initialize_survives() {
    let mut pools = MockRelayPool::create_linked_group(2);
    let server_pool = Arc::new(pools.remove(0));
    let client_pool = Arc::new(pools.remove(0));
    let server_pubkey = server_pool.mock_public_key();

    let transport = server_transport(&server_pool).await;
    let (handle, _ready) = spawn_server(SurvivalServer, transport);
    let_subscriptions_settle().await;

    let honest = Keys::generate();
    publish_from(
        &client_pool,
        &honest,
        server_pubkey,
        &initialized_notification(),
    )
    .await;
    publish_from(
        &client_pool,
        &honest,
        server_pubkey,
        &initialize_request("reorder-init"),
    )
    .await;
    publish_from(
        &client_pool,
        &honest,
        server_pubkey,
        &request("reorder-tools", "tools/list"),
    )
    .await;

    // The client's own request id is restored on the response, so it is the needle.
    wait_for_event_containing(
        &server_pool,
        server_pubkey,
        "reorder-tools",
        Duration::from_secs(5),
    )
    .await;

    handle.abort();
}

/// The same stranger notification, but with one inbound middleware registered, which routes every
/// message through a detached per-message task instead of the event loop's inline path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stranger_notification_with_middleware_survives() {
    let mut pools = MockRelayPool::create_linked_group(3);
    let server_pool = Arc::new(pools.remove(0));
    let client_pool = Arc::new(pools.remove(0));
    let stranger_pool = Arc::new(pools.remove(0));
    let server_pubkey = server_pool.mock_public_key();

    let mut transport = server_transport(&server_pool).await;
    transport.add_inbound_middleware(Arc::new(ForwardAll));
    let (handle, _ready) = spawn_server(SurvivalServer, transport);
    let_subscriptions_settle().await;

    let stranger = Keys::generate();
    publish_from(
        &stranger_pool,
        &stranger,
        server_pubkey,
        &initialized_notification(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_client_can_serve_and_list(&client_pool, server_pubkey).await;

    handle.abort();
}

/// The first client's `initialize` no longer runs rmcp's pre-service handshake, so it is answered
/// with the handler's declared protocol version rather than an echo of the version it requested.
/// Assert on the FIRST client. Every later client is answered by the already-running service,
/// which never negotiates, so a later client reads the declared version no matter what the
/// handshake does and would pin nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_client_older_protocol_version_not_negotiated() {
    let mut pools = MockRelayPool::create_linked_group(2);
    let server_pool = Arc::new(pools.remove(0));
    let client_pool = Arc::new(pools.remove(0));
    let server_pubkey = server_pool.mock_public_key();

    let transport = server_transport(&server_pool).await;
    let (handle, _ready) = spawn_server(SurvivalServer, transport);
    let_subscriptions_settle().await;

    let transport = client_transport(&client_pool, server_pubkey).await;
    let client = tokio::time::timeout(
        Duration::from_secs(5),
        SurvivalClient::requesting(ProtocolVersion::V_2024_11_05).serve(transport),
    )
    .await
    .expect("first client serve() timed out")
    .expect("first client serve() failed");

    let peer = client.peer_info().expect("peer info after serve()");
    assert_eq!(
        peer.protocol_version,
        ProtocolVersion::V_2025_11_25,
        "the first client must receive the handler's declared protocol version, not an echo of \
         the version it requested"
    );

    client.cancel().await.expect("client cancel");
    handle.abort();
}

/// The announcement carries the handler's `protocolVersion` and the capability lists follow it.
/// The worker's startup handshake must not disturb either, which is what this pins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announced_server_publishes_capabilities() {
    let server_pool = Arc::new(MockRelayPool::new());
    let transport = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_announced_server(true),
        as_pool(&server_pool),
    )
    .await
    .expect("create announced server transport");

    let (handle, _ready) = spawn_server(SurvivalServer, transport);

    let announcement = wait_for_kind(
        &server_pool,
        SERVER_ANNOUNCEMENT_KIND,
        Duration::from_secs(5),
    )
    .await
    .content;
    let parsed: serde_json::Value =
        serde_json::from_str(&announcement).expect("announcement content is JSON");
    assert_eq!(
        parsed.get("protocolVersion").and_then(|v| v.as_str()),
        Some(mcp_protocol_version()),
        "announcement protocolVersion changed: {announcement}"
    );

    // The capability lists still follow the announcement.
    wait_for_kind(&server_pool, TOOLS_LIST_KIND, Duration::from_secs(5)).await;

    assert_sentinel_never_hit_the_wire(&server_pool).await;

    handle.abort();
}
