//! CEP-22: rmcp-level oversized-transfer timeout e2e tests.
//!
//! Covers progress-aware idle/max-total timeout semantics, stripped-progress
//! forwarding with token-type restoration, watchdog reaping, and the
//! default-on roundtrip.
//!
//! Declared in `Cargo.toml` with `required-features = ["rmcp", "test-utils"]`
//! (same as `e2e_happy_path`) so plain `cargo test` skips it and stays green.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contextvm_sdk::core::constants::CTXVM_MESSAGES_KIND;
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::base::BaseTransport;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::oversized_transfer::{
    build_oversized_frames, OversizedFrame, OversizedSenderOptions, OversizedTransferConfig,
};
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use contextvm_sdk::{
    progress_aware_options, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    PeerRequestOptionsExt, RelayPoolTrait,
};
use nostr_sdk::prelude::*;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, RawContent,
    ServerCapabilities,
};
use rmcp::service::ServiceError;
use rmcp::{schemars, tool, tool_handler, tool_router, ClientHandler, ServerHandler, ServiceExt};

// ── harness ──────────────────────────────────────────────────────────────────

/// Let spawned event loops call `notifications()` before we publish anything.
async fn let_event_loops_start() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

/// A started client transport over `client_pool` with the given config — plus
/// its message receiver.
async fn start_client_with(
    client_pool: MockRelayPool,
    config: NostrClientTransportConfig,
) -> (
    NostrClientTransport,
    tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
) {
    let mut client = NostrClientTransport::with_relay_pool(
        config,
        Arc::new(client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create client transport");
    let rx = client.take_message_receiver().expect("client rx");
    client.start().await.expect("client start");
    let_event_loops_start().await;
    (client, rx)
}

/// [`start_client_with`] for the common case: plaintext, default timeouts,
/// the given oversized config.
async fn start_client(
    client_pool: MockRelayPool,
    server_pubkey: &PublicKey,
    oversized: OversizedTransferConfig,
) -> (
    NostrClientTransport,
    tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
) {
    start_client_with(
        client_pool,
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized),
    )
    .await
}

/// A plaintext `BaseTransport` over the (greybox) server's pool, used to
/// hand-publish frames to the client.
fn greybox_server_base(server_pool: &Arc<MockRelayPool>) -> BaseTransport {
    BaseTransport {
        relay_pool: Arc::clone(server_pool) as Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode::Disabled,
        is_connected: true,
    }
}

/// Publish one frame as a plaintext kind-25910 event with the given tags.
async fn publish_frame(
    base: &BaseTransport,
    recipient: &PublicKey,
    tags: &[Tag],
    frame: JsonRpcNotification,
) {
    base.send_mcp_message(
        &JsonRpcMessage::Notification(frame),
        recipient,
        CTXVM_MESSAGES_KIND,
        tags.to_vec(),
        Some(false),
        None,
    )
    .await
    .expect("publish frame");
}

/// Poll the shared store until an event matching `pred` lands; return its id.
async fn poll_for_event(
    pool: &MockRelayPool,
    what: &str,
    pred: impl Fn(&Event) -> bool,
) -> EventId {
    for _ in 0..200 {
        if let Some(event) = pool
            .stored_events()
            .await
            .iter()
            .find(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND) && pred(e))
        {
            return event.id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never reached the relay store");
}

/// Count stored kind-25910 events carrying an oversized `cvm` frame.
async fn count_oversized_frames(pool: &MockRelayPool) -> usize {
    pool.stored_events()
        .await
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .filter(|e| {
            serde_json::from_str::<serde_json::Value>(&e.content)
                .ok()
                .is_some_and(|v| v.get("params").and_then(|p| p.get("cvm")).is_some())
        })
        .count()
}

/// `true` when the event carries an oversized frame of the given `frameType`.
fn is_frame_of_type(event: &Event, frame_type: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(&event.content)
        .ok()
        .and_then(|v| {
            v.get("params")
                .and_then(|p| p.get("cvm"))
                .and_then(|c| c.get("frameType"))
                .and_then(|f| f.as_str().map(|f| f == frame_type))
        })
        .unwrap_or(false)
}

/// Receive the next client message within `ms`, panicking on timeout/close.
async fn recv_within(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    ms: u64,
    what: &str,
) -> JsonRpcMessage {
    tokio::time::timeout(Duration::from_millis(ms), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .expect("client channel closed")
}

/// Drain client messages for up to `ms`, returning the first *response* and
/// skipping stripped progress forwards. `None` when the window closes first.
async fn try_recv_response(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    ms: u64,
) -> Option<JsonRpcMessage> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return None,
            Ok(None) => panic!("client channel closed"),
            Ok(Some(msg)) if msg.is_response() => return Some(msg),
            Ok(Some(_)) => continue,
        }
    }
}

/// Assert `msg` is a stripped progress forward: right method, token restored
/// to `expected_token`, the expected `progress` slot, and no `cvm` payload.
fn assert_stripped_forward(
    msg: JsonRpcMessage,
    expected_token: &serde_json::Value,
    expected_progress: u64,
) {
    let JsonRpcMessage::Notification(n) = msg else {
        panic!("expected a stripped progress notification");
    };
    assert_eq!(n.method, "notifications/progress");
    let params = n.params.expect("forwarded progress has params");
    assert_eq!(
        &params["progressToken"], expected_token,
        "token must be restored to the original JSON value, got {params}"
    );
    assert_eq!(params["progress"], serde_json::json!(expected_progress));
    assert!(
        params.get("cvm").is_none(),
        "cvm payload must be stripped, got {params}"
    );
}

// ── rmcp full-stack fixtures ─────────────────────────────────────────────────

/// Wraps a pool so every publish lands `publish_delay` apart — deterministic
/// inter-frame gaps for an oversized response (à la `transport_integration`'s
/// `TestRelayPool::with_publish_delay`).
struct DelayedRelayPool {
    inner: Arc<MockRelayPool>,
    publish_delay: Duration,
}

#[async_trait]
impl RelayPoolTrait for DelayedRelayPool {
    async fn connect(&self, relay_urls: &[String]) -> contextvm_sdk::Result<()> {
        self.inner.connect(relay_urls).await
    }

    async fn disconnect(&self) -> contextvm_sdk::Result<()> {
        self.inner.disconnect().await
    }

    async fn publish_event(&self, event: &Event) -> contextvm_sdk::Result<EventId> {
        tokio::time::sleep(self.publish_delay).await;
        self.inner.publish_event(event).await
    }

    async fn publish(&self, builder: EventBuilder) -> contextvm_sdk::Result<EventId> {
        tokio::time::sleep(self.publish_delay).await;
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

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BigParams {
    len: usize,
}

/// rmcp server with one tool: `big(len)` returns a `len`-byte text, driving
/// the response over the oversized threshold on demand.
#[derive(Clone)]
struct BigServer {
    tool_router: ToolRouter<BigServer>,
}

impl BigServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl BigServer {
    #[tool(description = "Return a text payload of `len` bytes")]
    fn big(
        &self,
        Parameters(BigParams { len }): Parameters<BigParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(
            "B".repeat(len),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for BigServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("oversized-e2e-server", "0.1.0"))
    }
}

#[derive(Clone, Default)]
struct DemoClient;
impl ClientHandler for DemoClient {}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn call_params(name: &'static str, args: serde_json::Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Ok(v) = serde_json::from_value(args) {
        params = params.with_arguments(v);
    }
    params
}

/// Greybox: wait for the rmcp client's `tools/call` request event; return its
/// id (for response e-tagging) and its `_meta.progressToken` in wire-string
/// form (rmcp stamps JSON numbers; frames carry the stringified token).
async fn wait_for_rmcp_request(pool: &MockRelayPool) -> (EventId, String) {
    for _ in 0..500 {
        for event in pool.stored_events().await {
            if event.kind != Kind::Custom(CTXVM_MESSAGES_KIND) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.content) else {
                continue;
            };
            if v.get("method").and_then(|m| m.as_str()) != Some("tools/call") {
                continue;
            }
            let token = match v
                .get("params")
                .and_then(|p| p.get("_meta"))
                .and_then(|m| m.get("progressToken"))
            {
                Some(serde_json::Value::Number(n)) => n.to_string(),
                Some(serde_json::Value::String(s)) => s.clone(),
                _ => continue,
            };
            return (event.id, token);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("rmcp tools/call request never reached the relay store");
}

/// Assert the requester published a `notifications/cancelled` carrying
/// `reason` (the fork's timeout-cancel publication).
async fn assert_cancelled_with_reason(pool: &MockRelayPool, reason: &str) {
    for _ in 0..200 {
        for event in pool.stored_events().await {
            if event.kind != Kind::Custom(CTXVM_MESSAGES_KIND) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.content) else {
                continue;
            };
            if v.get("method").and_then(|m| m.as_str()) == Some("notifications/cancelled")
                && v.get("params")
                    .and_then(|p| p.get("reason"))
                    .and_then(|r| r.as_str())
                    == Some(reason)
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no notifications/cancelled with reason {reason:?} observed");
}

/// Greybox harness for the stalled/trickle timeout tests: a stateless rmcp
/// client (initialize emulated locally, so the test-driven "server" only ever
/// sees `tools/call`), with the
/// `call_tool_with_options` future spawned and timed from issue to settle.
async fn spawn_greybox_call(
    client_pool: MockRelayPool,
    server_pubkey: &PublicKey,
    idle: Duration,
    max_total: Duration,
) -> tokio::task::JoinHandle<(Result<CallToolResult, ServiceError>, Duration)> {
    let client_transport = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_stateless(true)
            .with_oversized_transfer(OversizedTransferConfig::enabled()),
        Arc::new(client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create client transport");

    let client = DemoClient
        .serve(client_transport)
        .await
        .expect("client init (stateless)");

    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        let result = client
            .peer()
            .call_tool_with_options(
                call_params("big", serde_json::json!({ "len": 1 })),
                progress_aware_options(idle, max_total),
            )
            .await;
        // Keep the running service alive until the call settles.
        drop(client);
        (result, started.elapsed())
    })
}

// ── harness smoke test ───────────────────────────────────────────────────────

/// Smoke test pinning the target's harness wiring: feature gates plus a linked
/// mock relay pair with distinct signing identities.
#[test]
fn harness_mock_relay_pair_is_linked() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    assert_ne!(
        client_pool.mock_public_key(),
        server_pool.mock_public_key(),
        "paired mock pools must have distinct signing identities"
    );
}

// ── stripped-progress forwarding ─────────────────────────────────────────────

/// Inbound start/chunk frames of an oversized response are forwarded to the
/// local consumer as stripped progress notifications whose `progressToken` is
/// restored to the JSON **number** recorded at send time (the wire carries the
/// stringified token, as every real sender emits); the `end` frame yields only
/// the reassembled response.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_progress_token_restored() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let (client, mut client_rx) = start_client(
        client_pool,
        &server_pubkey,
        OversizedTransferConfig::enabled(),
    )
    .await;

    // Request with the JSON number 7 as its token — the rmcp shape. The
    // transport records the original keyed by "7". Small request → single event.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("e4-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({ "_meta": { "progressToken": 7 } })),
    });
    client.send(&request).await.expect("send request");

    // Greybox server: pick up the request event for response correlation.
    let request_event_id = poll_for_event(&server_pool, "request event", |e| {
        serde_json::from_str::<serde_json::Value>(&e.content)
            .ok()
            .is_some_and(|v| v.get("method").and_then(|m| m.as_str()) == Some("tools/call"))
    })
    .await;

    // A 3-chunk response whose frames carry the realistic WIRE-STRING token
    // "7" (`into_progress_notification` stringifies; TS does the same).
    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("e4-1"),
        result: serde_json::json!({ "blob": "Z".repeat(90) }),
    });
    let serialized = serde_json::to_string(&response).unwrap();
    let frames = build_oversized_frames(
        &serialized,
        &OversizedSenderOptions::new("7").with_chunk_size(serialized.len().div_ceil(3)),
    )
    .unwrap();
    assert_eq!(frames.chunks.len(), 3, "harness wants exactly 3 chunks");

    let base = greybox_server_base(&server_pool);
    let tags = BaseTransport::create_response_tags(&client_pubkey, &request_event_id);
    for frame in frames.into_ordered() {
        publish_frame(&base, &client_pubkey, &tags, frame).await;
    }

    // Forward scope: start + each chunk (progress slots 1..=4, no handshake),
    // each restored to Number(7) — a verbatim wire clone would surface
    // String("7") and fail here.
    for expected_progress in 1u64..=4 {
        let msg = recv_within(&mut client_rx, 1000, "stripped progress forward").await;
        assert_stripped_forward(msg, &serde_json::json!(7), expected_progress);
    }

    // The end frame yields exactly the reassembled response…
    let msg = recv_within(&mut client_rx, 1000, "reassembled response").await;
    assert!(msg.is_response());
    assert_eq!(msg.id(), Some(&serde_json::json!("e4-1")));

    let extra = tokio::time::timeout(Duration::from_millis(150), client_rx.recv()).await;
    assert!(
        extra.is_err(),
        "no extra forward expected for the end frame"
    );
}

/// The server's `accept` for a client upload forwards exactly one stripped
/// progress notification (re-arming the requester's idle timer for the
/// response-wait phase), with the original JSON-number token restored from the
/// recorded value, and the blocked oversized send completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_frame_forwards_one_progress_reset() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    // Low threshold so a ~2 KB request fragments; server support is unknown
    // (no discovery yet) so the send blocks on the accept handshake.
    let (client, mut client_rx) = start_client(
        client_pool,
        &server_pubkey,
        OversizedTransferConfig::enabled()
            .with_threshold(600)
            .with_chunk_size(600),
    )
    .await;
    let client = Arc::new(client);

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("e5-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "_meta": { "progressToken": 8 },
            "blob": "Q".repeat(2000),
        })),
    });
    let send_task = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.send(&request).await })
    };

    // Greybox server: wait for the start frame, echo `accept` e-tagged to it.
    // The wire token is the stringified echo — exactly what a real server
    // emits (`into_progress_notification`).
    let start_event_id = poll_for_event(&server_pool, "start frame", |e| {
        is_frame_of_type(e, "start")
    })
    .await;
    let accept = OversizedFrame::Accept
        .into_progress_notification("8", 2, None)
        .expect("build accept frame");
    let tags = BaseTransport::create_response_tags(&client_pubkey, &start_event_id);
    publish_frame(
        &greybox_server_base(&server_pool),
        &client_pubkey,
        &tags,
        accept,
    )
    .await;

    // The blocked send unblocks and completes the upload.
    send_task
        .await
        .expect("send task join")
        .expect("oversized send completes after accept");

    // Exactly one stripped forward surfaced for the accept (slot 2), token
    // restored to the original JSON number.
    let msg = recv_within(&mut client_rx, 1000, "accept progress forward").await;
    assert_stripped_forward(msg, &serde_json::json!(8), 2);

    let extra = tokio::time::timeout(Duration::from_millis(150), client_rx.recv()).await;
    assert!(
        extra.is_err(),
        "exactly one forward expected for the accept handshake"
    );
}

// ── receiver-side watchdog ───────────────────────────────────────────────────

/// The client watchdog reaps a transfer stalled past its hard deadline; the
/// late remainder of the transfer is orphan-ignored (nothing delivered), and a
/// fresh transfer re-using the reaped token completes — proving the slot was
/// actually freed (duplicate-`start` would error if state lingered).
///
/// Timing: 200 ms deadline, 1 s sweep tick (correlation TTL 2 s → clamp
/// floor), 3 s stall = 3 sweep ticks and 15× the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watchdog_reaps_stalled_inbound_transfer_client() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let (_client, mut client_rx) = start_client_with(
        client_pool,
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_timeout(Duration::from_secs(2))
            .with_oversized_transfer(
                OversizedTransferConfig::enabled().with_transfer_timeout_ms(200),
            ),
    )
    .await;

    // An unsolicited 3-chunk inbound message keyed by token "w6" (delivery
    // does not require a pending request; frames only need the client p-tag).
    let payload = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("w6-1"),
        result: serde_json::json!({ "blob": "Y".repeat(90) }),
    });
    let serialized = serde_json::to_string(&payload).unwrap();
    let build = || {
        build_oversized_frames(
            &serialized,
            &OversizedSenderOptions::new("w6").with_chunk_size(serialized.len().div_ceil(3)),
        )
        .expect("build frames")
    };

    let base = greybox_server_base(&server_pool);
    let tags = BaseTransport::create_recipient_tags(&client_pubkey);

    // start + first chunk, then stall past the deadline and ≥ 3 sweep ticks.
    let mut stalled = build().into_ordered();
    let rest = stalled.split_off(2);
    for frame in stalled {
        publish_frame(&base, &client_pubkey, &tags, frame).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The late remainder hits a reaped slot: orphan-ignored, nothing surfaces.
    for frame in rest {
        publish_frame(&base, &client_pubkey, &tags, frame).await;
    }
    assert!(
        try_recv_response(&mut client_rx, 500).await.is_none(),
        "a reaped transfer must never deliver a message"
    );

    // The same token is re-admittable; a fresh full transfer completes.
    for frame in build().into_ordered() {
        publish_frame(&base, &client_pubkey, &tags, frame).await;
    }
    let delivered = try_recv_response(&mut client_rx, 1500)
        .await
        .expect("fresh same-token transfer must deliver after the reap");
    assert_eq!(delivered.id(), Some(&serde_json::json!("w6-1")));
}

/// Mirror of the client watchdog test against the server transport — its sweep
/// arm reaps the stalled per-peer transfer (and drops the now-empty receiver),
/// the late remainder is orphan-ignored, and a fresh same-token upload
/// reassembles into an `IncomingRequest` on the server receiver.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watchdog_reaps_stalled_inbound_transfer_server() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    // 200 ms hard deadline → 1 s sweep tick (clamp floor).
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(
                OversizedTransferConfig::enabled().with_transfer_timeout_ms(200),
            ),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");
    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    let_event_loops_start().await;

    // Greybox client upload: handshake-layout frames (the server emits an
    // accept to the unknown client; the test ignores it).
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("w7-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "_meta": { "progressToken": "w7" },
            "blob": "Q".repeat(300),
        })),
    });
    let serialized = serde_json::to_string(&request).unwrap();
    let build = || {
        build_oversized_frames(
            &serialized,
            &OversizedSenderOptions::new("w7")
                .with_chunk_size(96)
                .with_accept_handshake(true),
        )
        .expect("build frames")
    };

    let base = BaseTransport {
        relay_pool: Arc::new(client_pool) as Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode::Disabled,
        is_connected: true,
    };
    let tags = BaseTransport::create_recipient_tags(&server_pubkey);

    // start + first chunk, then stall past the deadline and ≥ 3 sweep ticks.
    let mut stalled = build().into_ordered();
    let rest = stalled.split_off(2);
    for frame in stalled {
        publish_frame(&base, &server_pubkey, &tags, frame).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Late remainder → reaped slot → orphan-ignored; no request surfaces.
    for frame in rest {
        publish_frame(&base, &server_pubkey, &tags, frame).await;
    }
    let nothing = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        nothing.is_err(),
        "a reaped transfer must never deliver a request"
    );

    // Fresh same-token upload completes (the swept-empty receiver was dropped
    // and is recreated on admission).
    for frame in build().into_ordered() {
        publish_frame(&base, &server_pubkey, &tags, frame).await;
    }
    let incoming = tokio::time::timeout(Duration::from_millis(1500), server_rx.recv())
        .await
        .expect("fresh same-token transfer must deliver after the reap")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/call"));
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("w7-1")));
}

// ── progress-aware request timeouts ──────────────────────────────────────────

/// Full-stack: a `call_tool_with_options` whose oversized response outlasts the
/// idle timeout several times over still succeeds, because every forwarded
/// chunk resets the idle timer. Without the forwarding seam this exact call
/// fails with `Timeout{400ms}`.
///
/// Timing: 150 ms publish delay vs 400 ms idle (≥ 2.5×); ~240 KB payload = ≥ 5
/// chunks at the 48 000 B default chunk size, pinned above the 65 535 B
/// single-event NIP-44 cap so an unfragmented path cannot pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_response_progress_resets_idle_timeout() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    // The server publishes through a 150 ms-delay pool: response frames land
    // one delay apart, so the full transfer takes ≥ 5 × 150 ms > idle.
    let delayed: Arc<dyn RelayPoolTrait> = Arc::new(DelayedRelayPool {
        inner: Arc::clone(&server_pool),
        publish_delay: Duration::from_millis(150),
    });
    let server_transport = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(OversizedTransferConfig::enabled()),
        delayed,
    )
    .await
    .expect("create server transport");

    let client_transport = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(OversizedTransferConfig::enabled()),
        Arc::new(client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create client transport");

    let server_handle = tokio::spawn(async move {
        let running = BigServer::new()
            .serve(server_transport)
            .await
            .expect("server serve failed");
        let _ = running.waiting().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let client = tokio::time::timeout(Duration::from_secs(10), DemoClient.serve(client_transport))
        .await
        .expect("client startup timed out")
        .expect("client init failed");

    let result = client
        .peer()
        .call_tool_with_options(
            call_params("big", serde_json::json!({ "len": 240_000 })),
            progress_aware_options(Duration::from_millis(400), Duration::from_secs(10)),
        )
        .await
        .expect("oversized call must succeed via per-chunk idle resets");
    assert_eq!(first_text(&result).len(), 240_000);

    // Fragmentation actually happened (not a single-event fluke).
    let frames = count_oversized_frames(&server_pool).await;
    assert!(frames >= 5, "expected ≥5 oversized frames, got {frames}");

    server_handle.abort();
}

/// Greybox, paused time: start + 2 chunks then silence — the idle timer, reset
/// by each forwarded frame, trips ~idle after the LAST chunk (not after request
/// issue), with `Timeout{timeout == idle}` and a published
/// `notifications/cancelled` carrying the idle reason.
#[tokio::test(start_paused = true)]
async fn oversized_stalled_transfer_trips_idle_timeout() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let idle = Duration::from_millis(400);
    let call = spawn_greybox_call(client_pool, &server_pubkey, idle, Duration::from_secs(10)).await;

    let (request_event_id, wire_token) = wait_for_rmcp_request(&server_pool).await;

    // A 5-chunk response; publish start + 2 chunks 200 ms apart, then stall.
    let payload = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(0),
        result: serde_json::json!({ "blob": "S".repeat(200) }),
    });
    let serialized = serde_json::to_string(&payload).unwrap();
    let frames = build_oversized_frames(
        &serialized,
        &OversizedSenderOptions::new(&wire_token).with_chunk_size(serialized.len().div_ceil(5)),
    )
    .expect("build frames");
    let base = greybox_server_base(&server_pool);
    let tags = BaseTransport::create_response_tags(&client_pubkey, &request_event_id);
    for frame in frames.into_ordered().into_iter().take(3) {
        publish_frame(&base, &client_pubkey, &tags, frame).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let (result, elapsed) = call.await.expect("join call task");
    match result {
        Err(ServiceError::Timeout { timeout }) => assert_eq!(timeout, idle, "idle timer fired"),
        other => panic!("expected idle Timeout, got {other:?}"),
    }
    // Two 200 ms-spaced resets pushed expiry well past issue+idle: the error
    // lands ~idle after the LAST frame (≈ 0.6 s publish phase + 0.4 s idle).
    assert!(
        elapsed >= Duration::from_millis(700),
        "resets must precede expiry; elapsed {elapsed:?}"
    );
    assert_cancelled_with_reason(&server_pool, "request timeout").await;
}

/// Greybox, paused time: chunks trickle every 150 ms — idle (400 ms) never
/// fires, but the max-total cap (1 s) does, despite continuous progress, with
/// `Timeout{timeout == max_total}` and the
/// max-total cancellation reason.
#[tokio::test(start_paused = true)]
async fn oversized_trickle_trips_max_total_timeout() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let idle = Duration::from_millis(400);
    let max_total = Duration::from_secs(1);
    let call = spawn_greybox_call(client_pool, &server_pubkey, idle, max_total).await;

    let (request_event_id, wire_token) = wait_for_rmcp_request(&server_pool).await;

    // ≥ 12 chunks available; publish start + chunks every 150 ms — the
    // trickle outlasts max_total while every gap stays under idle.
    let payload = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(0),
        result: serde_json::json!({ "blob": "T".repeat(500) }),
    });
    let serialized = serde_json::to_string(&payload).unwrap();
    let frames = build_oversized_frames(
        &serialized,
        &OversizedSenderOptions::new(&wire_token).with_chunk_size(40),
    )
    .expect("build frames");
    let base = greybox_server_base(&server_pool);
    let tags = BaseTransport::create_response_tags(&client_pubkey, &request_event_id);
    for frame in frames.into_ordered().into_iter().take(10) {
        publish_frame(&base, &client_pubkey, &tags, frame).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let (result, elapsed) = call.await.expect("join call task");
    match result {
        Err(ServiceError::Timeout { timeout }) => {
            assert_eq!(timeout, max_total, "max-total timer fired")
        }
        other => panic!("expected max-total Timeout, got {other:?}"),
    }
    // Fired at ~max_total despite continuous progress (publishes kept going
    // past it; the call settled on its own cap).
    assert!(
        elapsed >= Duration::from_millis(900) && elapsed <= Duration::from_millis(1400),
        "max-total should cap the call at ~1 s; elapsed {elapsed:?}"
    );
    assert_cancelled_with_reason(&server_pool, "maximum total timeout exceeded").await;
}

// ── default-on e2e roundtrip ─────────────────────────────────────────────────

/// `true` when the (plaintext) event carries a tag whose name is `name`.
fn event_has_tag(event: &Event, name: &str) -> bool {
    event
        .tags
        .iter()
        .any(|t| t.clone().to_vec().first().map(String::as_str) == Some(name))
}

/// With **no oversized config at all** — both transports on
/// defaults — a > 65 535-byte tool result roundtrips through the full rmcp
/// stack. That size exceeds the single-event NIP-44 plaintext cap, so the
/// roundtrip succeeding proves the server fragmented, which it only does after
/// learning support from the client's default-advertised tag. Capability
/// discovery is asserted on the wire (both directions); the transport's
/// `discovered_server_capabilities()` accessor itself is consumed by
/// `serve()`, and its transport-level behavior is already pinned by
/// `oversized_response_roundtrip_server_to_client`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_default_on_e2e_roundtrip() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    // Default configs: oversized transfer untouched (default-on is the point).
    // Plaintext mode so the stored events' discovery tags are inspectable.
    let server_transport = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");
    let client_transport = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled),
        Arc::new(client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create client transport");

    let server_handle = tokio::spawn(async move {
        let running = BigServer::new()
            .serve(server_transport)
            .await
            .expect("server serve failed");
        let _ = running.waiting().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let client = tokio::time::timeout(Duration::from_secs(10), DemoClient.serve(client_transport))
        .await
        .expect("client startup timed out")
        .expect("client init failed");

    // Plain `call_tool` — no helper, no options: the pure default experience.
    // (External timeout only bounds the test; plain calls have none.)
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        client.call_tool(call_params("big", serde_json::json!({ "len": 80_000 }))),
    )
    .await
    .expect("default-on oversized roundtrip timed out")
    .expect("call_tool failed");
    assert_eq!(
        first_text(&result).len(),
        80_000,
        "the >65 535-byte payload must roundtrip intact"
    );

    // Fragmentation actually happened (a single event cannot carry it).
    let frames = count_oversized_frames(&server_pool).await;
    assert!(
        frames >= 3,
        "expected at least start+chunk+end cvm frames, got {frames}"
    );

    // Both sides advertised by default: the client's first request and the
    // server's first response each carried the discovery tag — the learning
    // inputs for the response-fragmentation gate and the client capability
    // snapshot respectively.
    let events = server_pool.stored_events().await;
    assert!(
        events
            .iter()
            .any(|e| e.pubkey == client_pubkey && event_has_tag(e, "support_oversized_transfer")),
        "client's first request must advertise support_oversized_transfer"
    );
    assert!(
        events
            .iter()
            .any(|e| e.pubkey == server_pubkey && event_has_tag(e, "support_oversized_transfer")),
        "server's first response must advertise support_oversized_transfer"
    );

    server_handle.abort();
}
