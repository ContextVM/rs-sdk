//! End-to-end happy path integration tests.
//!
//! Exercises the full SDK stack in-memory: RMCP handler → NostrServerWorker →
//! NostrServerTransport → MockRelayPool → NostrClientTransport →
//! NostrClientWorker → RMCP client.  No network required.

#![cfg(feature = "rmcp")]

use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use contextvm_sdk::RelayPoolTrait;

use rmcp::{
    handler::server::router::tool::ToolRouter, handler::server::wrapper::Parameters, model::*,
    schemars, service::RequestContext, tool, tool_handler, tool_router, ClientHandler, RoleServer,
    ServerHandler, ServiceExt,
};
use tokio::sync::Mutex;

// ── Fixtures ──────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoParams {
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AddParams {
    a: i64,
    b: i64,
}

#[derive(Clone)]
struct DemoServer {
    echo_count: Arc<Mutex<u32>>,
    tool_router: ToolRouter<DemoServer>,
}

impl DemoServer {
    fn new() -> Self {
        Self {
            echo_count: Arc::new(Mutex::new(0)),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl DemoServer {
    #[tool(description = "Echo a message back")]
    async fn echo(
        &self,
        Parameters(EchoParams { message }): Parameters<EchoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut n = self.echo_count.lock().await;
        *n += 1;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Echo #{n}: {message}"
        ))]))
    }

    #[tool(description = "Add two integers")]
    fn add(
        &self,
        Parameters(AddParams { a, b }): Parameters<AddParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{a} + {b} = {}",
            a + b
        ))]))
    }

    #[tool(description = "Return total echo calls")]
    async fn get_echo_count(&self) -> Result<CallToolResult, ErrorData> {
        let n = self.echo_count.lock().await;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Total echo calls: {n}"
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for DemoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("e2e-test-server", "0.1.0"))
    }

    async fn list_resources(
        &self,
        _req: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![
                RawResource::new("demo://readme", "Demo README".to_string()).no_annotation()
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        req: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        match req.uri.as_str() {
            "demo://readme" => Ok(ReadResourceResult::new(vec![ResourceContents::text(
                "Demo content.",
                req.uri,
            )])),
            other => Err(ErrorData::resource_not_found(
                "not_found",
                Some(serde_json::json!({ "uri": other })),
            )),
        }
    }
}

#[derive(Clone, Default)]
struct DemoClient;
impl ClientHandler for DemoClient {}

// ── Helpers ───────────────────────────────────────────────────────────────

fn as_pool(pool: MockRelayPool) -> Arc<dyn RelayPoolTrait> {
    Arc::new(pool)
}

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

fn call_params(name: &'static str, args: Option<serde_json::Value>) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(v) = args.and_then(|v| serde_json::from_value(v).ok()) {
        params = params.with_arguments(v);
    }
    params
}

// ── Core scenario runner ──────────────────────────────────────────────────

async fn run_e2e_scenario(mode: EncryptionMode) {
    let (client_pool, server_pool) = MockRelayPool::create_pair();

    // Extract server pubkey before as_pool() takes ownership
    let server_pubkey_hex = server_pool.mock_public_key().to_hex();

    let server_transport = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(mode),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let client_transport = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_server_pubkey(server_pubkey_hex)
            .with_encryption_mode(mode)
            .with_relay_urls(vec!["wss://mock.relay".to_string()]),
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let server_handle = tokio::spawn(async move {
        DemoServer::new()
            .serve(server_transport)
            .await
            .expect("server serve failed")
            .waiting()
            .await
            .expect("server error");
    });

    // Let server event loop establish subscriptions
    tokio::time::sleep(Duration::from_millis(10)).await;

    // rmcp handles the initialize handshake
    let client = tokio::time::timeout(Duration::from_secs(5), DemoClient.serve(client_transport))
        .await
        .expect("client startup timed out")
        .expect("client init failed");

    let peer = client
        .peer_info()
        .expect("peer_info should be available after serve()");
    assert_eq!(
        peer.server_info.name, "e2e-test-server",
        "server name mismatch"
    );

    let tools = client.list_all_tools().await.expect("list_all_tools");
    assert_eq!(tools.len(), 3, "expected 3 tools");
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(tool_names.contains(&"echo"), "missing echo tool");
    assert!(tool_names.contains(&"add"), "missing add tool");
    assert!(
        tool_names.contains(&"get_echo_count"),
        "missing get_echo_count tool"
    );

    let echo1 = client
        .call_tool(call_params(
            "echo",
            Some(serde_json::json!({"message": "hello"})),
        ))
        .await
        .expect("echo call");
    let echo1_text = first_text(&echo1);
    assert!(
        echo1_text.contains("hello"),
        "echo should contain 'hello', got: {echo1_text}"
    );
    assert!(
        echo1_text.contains("#1"),
        "first echo should be #1, got: {echo1_text}"
    );

    let add = client
        .call_tool(call_params(
            "add",
            Some(serde_json::json!({"a": 7, "b": 5})),
        ))
        .await
        .expect("add call");
    let add_text = first_text(&add);
    assert!(
        add_text.contains("12"),
        "add result should contain 12, got: {add_text}"
    );

    let count = client
        .call_tool(call_params("get_echo_count", None))
        .await
        .expect("get_echo_count call");
    let count_text = first_text(&count);
    assert!(
        count_text.contains("calls: 1"),
        "echo count should be 1 after one echo call, got: {count_text}"
    );

    let echo2 = client
        .call_tool(call_params(
            "echo",
            Some(serde_json::json!({"message": "world"})),
        ))
        .await
        .expect("second echo call");
    let echo2_text = first_text(&echo2);
    assert!(
        echo2_text.contains("#2"),
        "second echo should be #2, got: {echo2_text}"
    );

    let count2 = client
        .call_tool(call_params("get_echo_count", None))
        .await
        .expect("get_echo_count after second echo");
    let count2_text = first_text(&count2);
    assert!(
        count2_text.contains("calls: 2"),
        "echo count should be 2, got: {count2_text}"
    );

    let resources = client
        .list_all_resources()
        .await
        .expect("list_all_resources");
    assert_eq!(resources.len(), 1, "expected 1 resource");
    assert_eq!(resources[0].uri.as_str(), "demo://readme");
    assert_eq!(resources[0].name.as_str(), "Demo README");

    let read_result = client
        .read_resource(ReadResourceRequestParams::new("demo://readme"))
        .await
        .expect("read_resource");
    assert_eq!(read_result.contents.len(), 1);
    match &read_result.contents[0] {
        ResourceContents::TextResourceContents { text, uri, .. } => {
            assert_eq!(uri, "demo://readme");
            assert!(
                text.contains("Demo content."),
                "unexpected resource text: {text}"
            );
        }
        _ => panic!("expected TextResourceContents"),
    }

    match client.call_tool(call_params("no_such_tool", None)).await {
        Err(_) => {}
        Ok(r) if r.is_error.unwrap_or(false) => {}
        Ok(_) => panic!("expected unknown tool to fail"),
    }

    client.cancel().await.expect("client cancel");
    server_handle.abort();
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_happy_path_encryption_disabled() {
    // Plaintext full-stack works since MockRelayPool started respecting
    // per-subscription filters (commit a0f9413): the server's own responses
    // are p-tagged to the client, so they no longer echo back into the
    // server's subscription as they did with the old broadcast-to-all mock.
    run_e2e_scenario(EncryptionMode::Disabled).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_happy_path_encryption_optional() {
    run_e2e_scenario(EncryptionMode::Optional).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_happy_path_encryption_required() {
    run_e2e_scenario(EncryptionMode::Required).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_multi_client_no_crosstalk() {
    let mut pools = MockRelayPool::create_linked_group(3);
    let server_pool = pools.remove(0);
    let client1_pool = pools.remove(0);
    let client2_pool = pools.remove(0);

    // Extract server pubkey BEFORE as_pool() takes ownership
    let server_pubkey_hex = server_pool.mock_public_key().to_hex();

    let server_transport = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Optional),
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let server_handle = tokio::spawn(async move {
        DemoServer::new()
            .serve(server_transport)
            .await
            .expect("server serve failed")
            .waiting()
            .await
            .expect("server error");
    });

    tokio::time::sleep(Duration::from_millis(10)).await;

    // Create two independent clients
    let client1_transport = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_server_pubkey(server_pubkey_hex.clone())
            .with_encryption_mode(EncryptionMode::Optional)
            .with_relay_urls(vec!["wss://mock.relay".to_string()]),
        as_pool(client1_pool),
    )
    .await
    .expect("create client1 transport");

    let client2_transport = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_server_pubkey(server_pubkey_hex)
            .with_encryption_mode(EncryptionMode::Optional)
            .with_relay_urls(vec!["wss://mock.relay".to_string()]),
        as_pool(client2_pool),
    )
    .await
    .expect("create client2 transport");

    let client1 = tokio::time::timeout(Duration::from_secs(5), DemoClient.serve(client1_transport))
        .await
        .expect("client1 timed out")
        .expect("client1 init");

    let client2 = tokio::time::timeout(Duration::from_secs(5), DemoClient.serve(client2_transport))
        .await
        .expect("client2 timed out")
        .expect("client2 init");

    // Both clients call echo — responses must route correctly
    let r1 = client1
        .call_tool(call_params(
            "echo",
            Some(serde_json::json!({"message": "from-client1"})),
        ))
        .await
        .expect("client1 echo");
    assert!(
        first_text(&r1).contains("from-client1"),
        "client1 should get its own response"
    );

    let r2 = client2
        .call_tool(call_params(
            "echo",
            Some(serde_json::json!({"message": "from-client2"})),
        ))
        .await
        .expect("client2 echo");
    assert!(
        first_text(&r2).contains("from-client2"),
        "client2 should get its own response"
    );

    // Shared stateful counter should reflect calls from both clients
    let count = client1
        .call_tool(call_params("get_echo_count", None))
        .await
        .expect("client1 get_echo_count");
    assert!(
        first_text(&count).contains("calls: 2"),
        "echo count should be 2 after calls from both clients"
    );

    // Cleanup
    client1.cancel().await.expect("client1 cancel");
    client2.cancel().await.expect("client2 cancel");
    server_handle.abort();
}
