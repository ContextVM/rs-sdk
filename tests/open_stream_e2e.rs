//! CEP-41 open-stream end-to-end tests — full rmcp client + server over the mock
//! relay, plus the CEP-22 (oversized request) + CEP-41 (streaming response)
//! composition.
//!
//! Declared in `Cargo.toml` with `required-features = ["rmcp", "test-utils"]`
//! (same as `e2e_happy_path`) so plain `cargo test` skips it and stays green.

use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::open_stream::{OpenStreamConfig, OpenStreamWriter};
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use contextvm_sdk::{
    call_tool_stream, progress_aware_options, JsonRpcMessage, JsonRpcRequest,
    PeerRequestOptionsExt, RelayPoolTrait, DEFAULT_OVERSIZED_IDLE_TIMEOUT,
    DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT,
};
use futures::StreamExt;
use nostr_sdk::prelude::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, RawContent,
    ServerCapabilities,
};
use rmcp::service::RequestContext;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ClientHandler, RoleServer, ServerHandler, ServiceExt,
};
use tokio::sync::Notify;

// ── harness ────────────────────────────────────────────────────────────────

/// A `big_data` response payload size that exceeds the default oversized
/// threshold (48_000 bytes), forcing CEP-22 fragmentation.
const BIG_RESPONSE_LEN: usize = 120_000;

fn as_pool(pool: MockRelayPool) -> Arc<dyn RelayPoolTrait> {
    Arc::new(pool)
}

async fn let_event_loops_start() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TopicParams {
    topic: String,
}

/// rmcp server exposing CEP-41 streaming tools. Each tool reaches its injected
/// [`OpenStreamWriter`] via `ctx.extensions`. `release` gates the `deferred`
/// tool's `close` so a test can observe response deferral.
#[derive(Clone)]
struct StreamServer {
    release: Arc<Notify>,
}

impl StreamServer {
    fn new(release: Arc<Notify>) -> Self {
        Self { release }
    }
}

fn writer_of(ctx: &RequestContext<RoleServer>) -> Option<OpenStreamWriter> {
    ctx.extensions.get::<OpenStreamWriter>().cloned()
}

#[tool_router]
impl StreamServer {
    /// Stream three chunks then close; the final result must arrive after `close`.
    #[tool(description = "Stream a, b, c then complete")]
    async fn stream3(
        &self,
        Parameters(TopicParams { topic }): Parameters<TopicParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(writer) = writer_of(&ctx) {
            let _ = writer.start().await;
            let _ = writer.write("a".to_string()).await;
            let _ = writer.write("b".to_string()).await;
            let _ = writer.write("c".to_string()).await;
            let _ = writer.close().await;
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "completed:{topic}"
        ))]))
    }

    /// Stream a, b, c then return the *length* of the received topic (a small
    /// response, so a large reassembled request does not force an oversized
    /// final response). Used by the CEP-22 + CEP-41 composition test.
    #[tool(description = "Stream a, b, c then return the received topic length")]
    async fn stream_len(
        &self,
        Parameters(TopicParams { topic }): Parameters<TopicParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(writer) = writer_of(&ctx) {
            let _ = writer.start().await;
            let _ = writer.write("a".to_string()).await;
            let _ = writer.write("b".to_string()).await;
            let _ = writer.write("c".to_string()).await;
            let _ = writer.close().await;
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "len:{}",
            topic.len()
        ))]))
    }

    /// Stream one chunk, block until released, then close — proves the final
    /// result is held until the stream closes.
    #[tool(description = "Stream then block until released, then close")]
    async fn deferred(
        &self,
        Parameters(TopicParams { topic }): Parameters<TopicParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(writer) = writer_of(&ctx) {
            let _ = writer.start().await;
            let _ = writer.write(format!("{topic}:1")).await;
            self.release.notified().await;
            let _ = writer.close().await;
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "deferred:{topic}"
        ))]))
    }

    /// Carries a progress token but never streams — the response must be sent
    /// normally (the unstarted-writer / progress-token-conflict guard).
    #[tool(description = "Return without streaming")]
    async fn no_stream(
        &self,
        Parameters(TopicParams { topic }): Parameters<TopicParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(format!(
            "plain:{topic}"
        ))]))
    }

    /// Stream one chunk then stay open until the client aborts (the writer goes
    /// inactive when the server applies the inbound `abort`).
    #[tool(description = "Stream then wait for the client to abort")]
    async fn client_abortable(
        &self,
        Parameters(TopicParams { topic }): Parameters<TopicParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(writer) = writer_of(&ctx) {
            let _ = writer.start().await;
            let _ = writer.write(format!("{topic}:1")).await;
            for _ in 0..1000 {
                if !writer.is_active() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "client-aborted:{topic}"
        ))]))
    }

    /// Stream two topic-specific chunks then close — chunks carry the topic so a
    /// concurrent test can detect any token/stream crossing.
    #[tool(description = "Stream {topic}:1, {topic}:2 then complete")]
    async fn stream_topic(
        &self,
        Parameters(TopicParams { topic }): Parameters<TopicParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(writer) = writer_of(&ctx) {
            let _ = writer.start().await;
            let _ = writer.write(format!("{topic}:1")).await;
            let _ = writer.write(format!("{topic}:2")).await;
            let _ = writer.close().await;
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "completed:{topic}"
        ))]))
    }

    /// Stream `first:{topic}`, block until released, then stream `second:{topic}`
    /// and close. Lets a test interleave another call between the two chunks.
    #[tool(description = "Stream first, wait for release, stream second, close")]
    async fn stream_pair(
        &self,
        Parameters(TopicParams { topic }): Parameters<TopicParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(writer) = writer_of(&ctx) {
            let _ = writer.start().await;
            let _ = writer.write(format!("first:{topic}")).await;
            self.release.notified().await;
            let _ = writer.write(format!("second:{topic}")).await;
            let _ = writer.close().await;
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "done:{topic}"
        ))]))
    }

    /// Return a large (> oversized threshold) response with no streaming — used to
    /// exercise a CEP-22 oversized response while a separate CEP-41 stream is live.
    #[tool(description = "Return a large response payload without streaming")]
    async fn big_data(
        &self,
        Parameters(TopicParams { topic: _ }): Parameters<TopicParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(
            "X".repeat(BIG_RESPONSE_LEN),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for StreamServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("open-stream-e2e-server", "0.1.0"))
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

/// Whether any event stored on `relay` carries a `params.cvm.type == kind` frame.
/// The fixtures disable encryption, so `event.content` is plaintext JSON.
async fn relay_has_cvm_frame(relay: &MockRelayPool, kind: &str) -> bool {
    relay.stored_events().await.iter().any(|event| {
        serde_json::from_str::<serde_json::Value>(&event.content)
            .ok()
            .and_then(|v| {
                v.get("params")
                    .and_then(|p| p.get("cvm"))
                    .and_then(|c| c.get("type"))
                    .and_then(|t| t.as_str())
                    .map(|t| t == kind)
            })
            .unwrap_or(false)
    })
}

fn call_params(name: &'static str, topic: &str) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Ok(v) = serde_json::from_value(serde_json::json!({ "topic": topic })) {
        params = params.with_arguments(v);
    }
    params
}

struct Fixture {
    client: rmcp::service::RunningService<rmcp::RoleClient, DemoClient>,
    handle: contextvm_sdk::ClientOpenStreamHandle,
    server_handle: tokio::task::JoinHandle<()>,
    relay: Arc<MockRelayPool>,
    release: Arc<Notify>,
}

/// Build a running rmcp server (open-stream `server_enabled`) + a running rmcp
/// client, returning the client peer wrapper and the client's open-stream handle
/// (captured before the transport is moved into `serve`).
async fn fixture(server_enabled: bool, client_enabled: bool, oversized: bool) -> Fixture {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey_hex = server_pool.mock_public_key().to_hex();
    let server_pool = Arc::new(server_pool);
    let relay = server_pool.clone();
    let release = Arc::new(Notify::new());

    let server_transport = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_enabled(oversized)
            .with_open_stream(OpenStreamConfig::default().with_enabled(server_enabled)),
        server_pool as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let client_transport = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_server_pubkey(server_pubkey_hex)
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_oversized_enabled(oversized)
            .with_open_stream(OpenStreamConfig::default().with_enabled(client_enabled)),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    // Capture the open-stream handle BEFORE `serve` consumes the transport.
    let handle = client_transport.open_stream_handle();

    let server = StreamServer::new(release.clone());
    let server_handle = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server serve failed")
            .waiting()
            .await
            .expect("server error");
    });
    let_event_loops_start().await;

    let client = tokio::time::timeout(Duration::from_secs(5), DemoClient.serve(client_transport))
        .await
        .expect("client startup timed out")
        .expect("client init failed");

    Fixture {
        client,
        handle,
        server_handle,
        relay,
        release,
    }
}

async fn shutdown(fixture: Fixture) {
    let _ = fixture.client.cancel().await;
    fixture.server_handle.abort();
}

async fn collect_chunks(
    stream: &mut contextvm_sdk::transport::open_stream::OpenStreamSession,
) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(value) => out.push(value),
            Err(error) => panic!("stream yielded an error: {error}"),
        }
    }
    out
}

// ── tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_roundtrip_numeric_token() {
    let fx = fixture(true, true, false).await;

    let mut call = call_tool_stream(
        fx.client.peer(),
        &fx.handle,
        call_params("stream3", "orders"),
    )
    .await
    .expect("call_tool_stream");

    // rmcp stamps a numeric progressToken (wire-stringified into the frames).
    assert!(
        call.progress_token.parse::<u64>().is_ok(),
        "expected a numeric (stringified) progress token, got {:?}",
        call.progress_token
    );

    let chunks = collect_chunks(&mut call.stream).await;
    assert_eq!(chunks, vec!["a", "b", "c"]);

    let result = tokio::time::timeout(Duration::from_secs(5), &mut call.result)
        .await
        .expect("result timed out")
        .expect("tool call failed");
    assert_eq!(first_text(&result), "completed:orders");

    shutdown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_concurrent_calls_stay_isolated_by_token() {
    // Two `call_tool_stream` calls issued concurrently must not cross their
    // tokens/streams (the placeholder push→bind window is serialized). Topic-
    // specific chunks make any crossing observable.
    let fx = fixture(true, true, false).await;

    let (orders, invoices) = tokio::join!(
        call_tool_stream(
            fx.client.peer(),
            &fx.handle,
            call_params("stream_topic", "orders"),
        ),
        call_tool_stream(
            fx.client.peer(),
            &fx.handle,
            call_params("stream_topic", "invoices"),
        ),
    );
    let mut orders = orders.expect("orders call_tool_stream");
    let mut invoices = invoices.expect("invoices call_tool_stream");

    assert_ne!(
        orders.progress_token, invoices.progress_token,
        "concurrent calls must get distinct tokens"
    );

    let order_chunks = collect_chunks(&mut orders.stream).await;
    let invoice_chunks = collect_chunks(&mut invoices.stream).await;
    assert_eq!(order_chunks, vec!["orders:1", "orders:2"]);
    assert_eq!(invoice_chunks, vec!["invoices:1", "invoices:2"]);

    let order_result = tokio::time::timeout(Duration::from_secs(5), &mut orders.result)
        .await
        .expect("orders result timed out")
        .expect("orders tool failed");
    let invoice_result = tokio::time::timeout(Duration::from_secs(5), &mut invoices.result)
        .await
        .expect("invoices result timed out")
        .expect("invoices tool failed");
    assert_eq!(first_text(&order_result), "completed:orders");
    assert_eq!(first_text(&invoice_result), "completed:invoices");

    shutdown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_deferred_response_after_close() {
    let fx = fixture(true, true, false).await;

    let mut call = call_tool_stream(
        fx.client.peer(),
        &fx.handle,
        call_params("deferred", "orders"),
    )
    .await
    .expect("call_tool_stream");

    // First chunk arrives while the tool is blocked before `close`.
    let first = call.stream.next().await.expect("first chunk").expect("ok");
    assert_eq!(first, "orders:1");

    // The final result must NOT be ready while the stream is still open.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut call.result)
            .await
            .is_err(),
        "the final response must be deferred until the stream closes"
    );

    // Release the tool → it closes the stream → the deferred response flushes.
    fx.release.notify_one();

    assert!(
        call.stream.next().await.is_none(),
        "stream must close after release"
    );
    let result = tokio::time::timeout(Duration::from_secs(5), &mut call.result)
        .await
        .expect("result timed out")
        .expect("tool call failed");
    assert_eq!(first_text(&result), "deferred:orders");

    shutdown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_client_abort_propagates() {
    let fx = fixture(true, true, false).await;

    let mut call = call_tool_stream(
        fx.client.peer(),
        &fx.handle,
        call_params("client_abortable", "orders"),
    )
    .await
    .expect("call_tool_stream");

    let first = call.stream.next().await.expect("first chunk").expect("ok");
    assert_eq!(first, "orders:1");

    // Consumer cancels → publishes an `abort` to the server, whose writer aborts.
    call.abort(Some("client cancelled".to_string())).await;

    // The local stream surfaces the terminal abort error.
    match call.stream.next().await {
        Some(Err(error)) => assert!(error.to_string().contains("client cancelled")),
        other => panic!("expected an abort error, got {other:?}"),
    }

    // (Registry-slot freeing on consumer abort is unit-tested in
    // `open_stream/registry.rs::consumer_abort_frees_slot_and_runs_hook`.)

    // The server tool observed the abort (its writer went inactive) and returned.
    let result = tokio::time::timeout(Duration::from_secs(5), &mut call.result)
        .await
        .expect("result timed out")
        .expect("tool call failed");
    assert_eq!(first_text(&result), "client-aborted:orders");

    shutdown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_unstarted_writer_sends_normal_response() {
    let fx = fixture(true, true, false).await;

    let mut call = call_tool_stream(
        fx.client.peer(),
        &fx.handle,
        call_params("no_stream", "orders"),
    )
    .await
    .expect("call_tool_stream");

    // The tool never streamed: the response is sent normally (no deferral hang).
    let result = tokio::time::timeout(Duration::from_secs(5), &mut call.result)
        .await
        .expect("response must not hang when the writer never started")
        .expect("tool call failed");
    assert_eq!(first_text(&result), "plain:orders");

    drop(call.stream);
    shutdown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_gate_off_server_disabled_streams_nothing() {
    // Server has open-stream disabled: no writer is injected, so the tool cannot
    // stream — the response is plain and no open-stream frame ever hits the relay.
    let fx = fixture(false, true, false).await;

    let mut call = call_tool_stream(
        fx.client.peer(),
        &fx.handle,
        call_params("stream3", "orders"),
    )
    .await
    .expect("call_tool_stream");

    let result = tokio::time::timeout(Duration::from_secs(5), &mut call.result)
        .await
        .expect("result timed out")
        .expect("tool call failed");
    assert_eq!(first_text(&result), "completed:orders");

    // No open-stream cvm frame was ever published.
    let saw_open_stream_frame = fx.relay.stored_events().await.iter().any(|event| {
        serde_json::from_str::<serde_json::Value>(&event.content)
            .ok()
            .and_then(|v| {
                v.get("params")
                    .and_then(|p| p.get("cvm"))
                    .and_then(|c| c.get("type"))
                    .and_then(|t| t.as_str())
                    .map(|t| t == "open-stream")
            })
            .unwrap_or(false)
    });
    assert!(
        !saw_open_stream_frame,
        "a gated-off server must never publish open-stream frames"
    );

    drop(call.stream);
    shutdown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_roundtrip_string_token_greybox() {
    // A genuine STRING progressToken (rmcp always stamps numeric ones) driven
    // greybox: a raw client crafts the `tools/call`, the real server streams.
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let release = Arc::new(Notify::new());

    let server_transport = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_open_stream(OpenStreamConfig::enabled()),
        as_pool(server_pool),
    )
    .await
    .expect("server transport");
    let server = StreamServer::new(release);
    let server_handle = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server serve")
            .waiting()
            .await
            .expect("server error");
    });
    let_event_loops_start().await;

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_open_stream(OpenStreamConfig::enabled()),
        as_pool(client_pool),
    )
    .await
    .expect("client transport");
    let _client_rx = client.take_message_receiver().expect("client rx");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Bind a reader session to a string token, then publish a crafted tools/call.
    let pending = client.prepare_outbound_open_stream_session();
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "stream3",
            "arguments": { "topic": "orders" },
            "_meta": { "progressToken": "string-token-1" },
        })),
    });
    client.send(&request).await.expect("send tools/call");

    let (token, mut stream) = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("placeholder timed out")
        .expect("placeholder dropped")
        .expect("session admission");
    assert_eq!(token, "string-token-1");

    let chunks = collect_chunks(&mut stream).await;
    assert_eq!(chunks, vec!["a", "b", "c"]);

    let _ = client.close().await;
    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_oversized_request_streaming_response_composition() {
    // CEP-22 (oversized request) + CEP-41 (streaming response) over one token.
    let fx = fixture(true, true, true).await;

    // A large topic forces the request over the oversized threshold (CEP-22),
    // while the tool streams its response (CEP-41). `stream_len` echoes the
    // received length so the final response stays small.
    let big_len = 120_000usize;
    let big_topic = "Z".repeat(big_len);
    let mut call = call_tool_stream(
        fx.client.peer(),
        &fx.handle,
        call_params("stream_len", &big_topic),
    )
    .await
    .expect("call_tool_stream");

    // The streaming response is delivered in order (open-stream receiver), and is
    // not cross-fed by the oversized receiver (type-disjoint predicates).
    let chunks = collect_chunks(&mut call.stream).await;
    assert_eq!(chunks, vec!["a", "b", "c"]);

    // The request reassembled (CEP-22): the tool saw the full topic, and the final
    // response arrives after `close` (deferral).
    let result = tokio::time::timeout(Duration::from_secs(10), &mut call.result)
        .await
        .expect("result timed out")
        .expect("tool call failed");
    assert_eq!(first_text(&result), format!("len:{big_len}"));

    shutdown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_stream_oversized_response_while_separate_stream_is_live() {
    // CEP-22 × CEP-41 composition under concurrency: a live open stream (one
    // token) coexists with a separate plain tool whose oversized response (a
    // different token) is fragmented. The big tool's progress token creates an
    // *unused* server writer that must NOT defer its response (Fix 1) and must
    // NOT steal the live stream's session (Fix 2).
    let fx = fixture(true, true, true).await;

    // (1) Start the long-lived stream and read its first chunk.
    let mut call = call_tool_stream(
        fx.client.peer(),
        &fx.handle,
        call_params("stream_pair", "orders"),
    )
    .await
    .expect("call_tool_stream");
    let first = call.stream.next().await.expect("first chunk").expect("ok");
    assert_eq!(first, "first:orders");

    // (2) While the stream is live, a separate plain call returns an oversized
    //     response. Progress-aware options stamp a progressToken (the trigger for
    //     both the oversized routing and the unused-writer hazard).
    let big_result = fx
        .client
        .peer()
        .call_tool_with_options(
            call_params("big_data", "ignored"),
            progress_aware_options(
                DEFAULT_OVERSIZED_IDLE_TIMEOUT,
                DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT,
            ),
        )
        .await
        .expect("big_data call");
    let big_text = first_text(&big_result);
    assert_eq!(
        big_text.len(),
        BIG_RESPONSE_LEN,
        "oversized response reassembled byte-exactly"
    );
    assert!(
        big_text.bytes().all(|b| b == b'X'),
        "payload integrity preserved"
    );

    // (3) The oversized path must actually have fragmented the response.
    assert!(
        relay_has_cvm_frame(&fx.relay, "oversized-transfer").await,
        "the big response must have been fragmented via CEP-22"
    );

    // (4) The streaming session is unaffected: it delivers its remaining chunk,
    //     closes, and resolves its final result.
    fx.release.notify_one();
    let second = call.stream.next().await.expect("second chunk").expect("ok");
    assert_eq!(second, "second:orders");
    assert!(
        call.stream.next().await.is_none(),
        "stream must close after release"
    );
    let result = tokio::time::timeout(Duration::from_secs(10), &mut call.result)
        .await
        .expect("result timed out")
        .expect("tool call failed");
    assert_eq!(first_text(&result), "done:orders");

    shutdown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_call_with_progress_token_does_not_interfere_with_live_stream() {
    // Progress-token-conflict guard: a plain (non-streaming) call that carries a
    // progressToken returns normally — its unused server writer must not defer
    // the response — while a separate live stream keeps delivering uncrossed.
    let fx = fixture(true, true, false).await;

    // (1) Start the long-lived stream and read its first chunk.
    let mut call = call_tool_stream(
        fx.client.peer(),
        &fx.handle,
        call_params("stream_pair", "orders"),
    )
    .await
    .expect("call_tool_stream");
    let first = call.stream.next().await.expect("first chunk").expect("ok");
    assert_eq!(first, "first:orders");

    // (2) A plain call with progress-aware options (which inject a progressToken)
    //     must return its own result and must not be deferred forever.
    let plain = fx
        .client
        .peer()
        .call_tool_with_options(
            call_params("no_stream", "ping"),
            progress_aware_options(
                DEFAULT_OVERSIZED_IDLE_TIMEOUT,
                DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT,
            ),
        )
        .await
        .expect("plain call must not hang");
    assert_eq!(first_text(&plain), "plain:ping");

    // (3) The live stream received only its own chunks and still completes.
    fx.release.notify_one();
    let second = call.stream.next().await.expect("second chunk").expect("ok");
    assert_eq!(second, "second:orders");
    assert!(
        call.stream.next().await.is_none(),
        "stream must close after release"
    );
    let result = tokio::time::timeout(Duration::from_secs(5), &mut call.result)
        .await
        .expect("result timed out")
        .expect("tool call failed");
    assert_eq!(first_text(&result), "done:orders");

    shutdown(fx).await;
}
