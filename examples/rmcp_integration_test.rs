//! Comprehensive rmcp integration matrix for ContextVM SDK.
//!
//! This example validates three scenarios:
//! 1) local rmcp transport (in-process duplex)
//! 2) hybrid relay mode (rmcp server + legacy JSON-RPC client)
//! 3) full rmcp over relays (rmcp server + rmcp client)
//!
//! Run:
//!   cargo run --example rmcp_integration_test --features rmcp
//!   cargo run --example rmcp_integration_test --features rmcp -- local
//!   cargo run --example rmcp_integration_test --features rmcp -- hybrid
//!   cargo run --example rmcp_integration_test --features rmcp -- relay-rmcp
//!   cargo run --example rmcp_integration_test --features rmcp -- all
//!
//! Optional relay override:
//!   CTXVM_RELAY_URL=wss://relay.primal.net cargo run --example rmcp_integration_test --features rmcp -- all
//!   cargo run --example rmcp_integration_test --features rmcp -- all wss://relay.primal.net

use anyhow::{anyhow, bail, Context, Result};
use contextvm_sdk::core::constants::mcp_protocol_version;
use contextvm_sdk::core::types::{
    EncryptionMode, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    ServerInfo as CtxServerInfo,
};
use contextvm_sdk::gateway::{GatewayConfig, NostrMCPGateway};
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;
use contextvm_sdk::transport::server::NostrServerTransportConfig;
use rmcp::{
    handler::server::wrapper::Parameters, model::*, schemars, service::RequestContext, tool,
    tool_handler, tool_router, ClientHandler, RoleServer, ServerHandler, ServiceExt,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

const DEFAULT_RELAY_URL: &str = "wss://relay.primal.net";
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const RELAY_WARMUP: Duration = Duration::from_secs(2);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Local,
    Hybrid,
    RelayRmcp,
    All,
}

impl Mode {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("all") {
            "local" => Ok(Self::Local),
            "hybrid" => Ok(Self::Hybrid),
            "relay-rmcp" => Ok(Self::RelayRmcp),
            "all" => Ok(Self::All),
            other => bail!("Unknown mode '{other}'. Use one of: local | hybrid | relay-rmcp | all"),
        }
    }

    fn run_local(self) -> bool {
        matches!(self, Self::Local | Self::All)
    }

    fn run_hybrid(self) -> bool {
        matches!(self, Self::Hybrid | Self::All)
    }

    fn run_relay_rmcp(self) -> bool {
        matches!(self, Self::RelayRmcp | Self::All)
    }
}

// Parameter structs with JSON schema for tools/list.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoParams {
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AddParams {
    a: i64,
    b: i64,
}

use rmcp::handler::server::router::tool::ToolRouter;

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
    #[tool(description = "Echo a message back unchanged")]
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

    #[tool(description = "Add two integers and return their sum")]
    fn add(
        &self,
        Parameters(AddParams { a, b }): Parameters<AddParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{a} + {b} = {}",
            a + b
        ))]))
    }

    #[tool(description = "Return the total number of echo calls made so far")]
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
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "contextvm-demo".to_string(),
                title: Some("ContextVM Demo Server".to_string()),
                version: "0.1.0".to_string(),
                description: Some("Demonstrates rmcp integration over ContextVM".to_string()),
                icons: None,
                website_url: None,
            },
            instructions: Some("Try: echo, add, get_echo_count".to_string()),
        }
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
            "demo://readme" => Ok(ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "This server demonstrates the ContextVM rmcp integration.",
                    req.uri,
                )],
            }),
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

#[derive(Clone, Default)]
struct RelayRmcpClient;
impl ClientHandler for RelayRmcpClient {}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rmcp=warn".parse()?)
                .add_directive("contextvm_sdk=info".parse()?),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = Mode::parse(args.first().map(String::as_str))?;
    let relay_url = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("CTXVM_RELAY_URL").ok())
        .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string());

    println!("========================================");
    println!("ContextVM SDK rmcp integration matrix");
    println!("mode: {:?}", mode);
    println!("relay: {relay_url}");
    println!("========================================\n");

    if mode.run_local() {
        run_local_rmcp_case().await?;
    }

    if mode.run_hybrid() {
        run_hybrid_relay_case(&relay_url).await?;
    }

    if mode.run_relay_rmcp() {
        run_relay_rmcp_case(&relay_url).await?;
    }

    println!("\nAll selected integration scenarios passed.");
    Ok(())
}

async fn run_local_rmcp_case() -> Result<()> {
    println!("[local-rmcp] start");

    let (server_io, client_io) = tokio::io::duplex(65536);

    let server_handle = tokio::spawn(async move {
        DemoServer::new()
            .serve(server_io)
            .await
            .expect("server serve failed")
            .waiting()
            .await
            .expect("server error");
    });

    let client = DemoClient.serve(client_io).await?;

    let tools = client.list_all_tools().await?;
    assert_eq!(tools.len(), 3, "expected 3 tools in local rmcp case");

    let add_result = client
        .call_tool(call_params(
            "add",
            Some(serde_json::json!({ "a": 7, "b": 5 })),
        ))
        .await?;
    let add_text = first_text(&add_result);
    assert!(add_text.contains("12"), "expected add result to include 12");

    let resources = client.list_all_resources().await?;
    assert_eq!(
        resources.len(),
        1,
        "expected one resource in local rmcp case"
    );

    match client.call_tool(call_params("no_such_tool", None)).await {
        Err(_) => {}
        Ok(r) if r.is_error.unwrap_or(false) => {}
        Ok(_) => bail!("expected unknown tool to fail in local rmcp case"),
    }

    client.cancel().await?;
    server_handle.abort();

    println!("[local-rmcp] pass");
    Ok(())
}

async fn run_hybrid_relay_case(relay_url: &str) -> Result<()> {
    println!("[relay-hybrid] start (rmcp server + legacy client)");

    let server_keys = signer::generate();
    let server_pubkey_hex = server_keys.public_key().to_hex();

    println!("[relay-hybrid] stage: spawning rmcp server task");
    let relay_url_owned = relay_url.to_string();
    let server_task = tokio::spawn(async move {
        let server = NostrMCPGateway::serve_handler(
            server_keys,
            server_config(&relay_url_owned),
            DemoServer::new(),
        )
        .await
        .with_context(|| format!("failed to start rmcp server on relay {relay_url_owned}"))?;

        let _ = server
            .waiting()
            .await
            .map_err(|e| anyhow!("rmcp server exited with error: {e}"))?;

        Err(anyhow!("rmcp server stopped unexpectedly"))
    });

    sleep(RELAY_WARMUP).await;

    if server_task.is_finished() {
        let res = server_task
            .await
            .map_err(|e| anyhow!("rmcp server task join error: {e}"))?;
        return res.context("rmcp server task ended before client startup");
    }

    let outcome: Result<()> = async {
        println!("[relay-hybrid] stage: creating legacy proxy client");

        let mut proxy = timeout(
            STARTUP_TIMEOUT,
            NostrMCPProxy::new(
                signer::generate(),
                client_config(relay_url, server_pubkey_hex.clone()),
            ),
        )
        .await
        .with_context(|| {
            format!(
                "timed out creating legacy proxy client after {:?}",
                STARTUP_TIMEOUT
            )
        })?
        .context("failed to create legacy proxy client")?;

        println!("[relay-hybrid] stage: starting legacy proxy transport");
        let mut rx = timeout(STARTUP_TIMEOUT, proxy.start())
            .await
            .with_context(|| {
                format!(
                    "timed out starting legacy proxy transport after {:?}",
                    STARTUP_TIMEOUT
                )
            })?
            .context("failed to start legacy proxy")?;
        println!("[relay-hybrid] stage: legacy proxy started");

        let init_id = serde_json::json!(1);
        let init_request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: init_id.clone(),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": mcp_protocol_version(),
                "capabilities": {
                    "tools": {},
                    "resources": {}
                },
                "clientInfo": {
                    "name": "legacy-hybrid-client",
                    "version": "0.1.0"
                }
            })),
        });

        let init_response =
            send_legacy_request_and_wait(&proxy, &mut rx, init_request, &init_id).await?;
        assert_initialize_shape(&init_response)?;

        proxy
            .send(&JsonRpcMessage::Notification(JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "notifications/initialized".to_string(),
                params: None,
            }))
            .await
            .context("failed to send initialized notification")?;

        let tools_id = serde_json::json!(2);
        let tools_request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: tools_id.clone(),
            method: "tools/list".to_string(),
            params: Some(serde_json::json!({})),
        });

        let tools_response =
            send_legacy_request_and_wait(&proxy, &mut rx, tools_request, &tools_id).await?;
        let tools = extract_tools_list(&tools_response)?;
        assert!(
            tools
                .iter()
                .any(|t| t.get("name") == Some(&serde_json::json!("echo"))),
            "expected echo tool in hybrid case"
        );

        let call_id = serde_json::json!(3);
        let call_request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: call_id.clone(),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "echo",
                "arguments": { "message": "legacy-client-hello" }
            })),
        });

        let call_response = send_legacy_request_and_wait(&proxy, &mut rx, call_request, &call_id)
            .await
            .context("tools/call failed in hybrid case")?;
        let echo_text = extract_first_content_text(&call_response)?;
        assert!(
            echo_text.contains("legacy-client-hello"),
            "unexpected echo output in hybrid case: {echo_text}"
        );

        let unknown_id = serde_json::json!(4);
        let unknown_request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: unknown_id.clone(),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "no_such_tool",
                "arguments": {}
            })),
        });

        let unknown_response =
            send_legacy_request_and_wait(&proxy, &mut rx, unknown_request, &unknown_id).await?;
        assert_error_response(&unknown_response)?;

        proxy.stop().await.context("failed to stop legacy proxy")?;

        Ok(())
    }
    .await;

    server_task.abort();

    if server_task.is_finished() {
        let _ = server_task.await;
    }

    outcome?;

    println!("[relay-hybrid] pass");
    Ok(())
}

async fn run_relay_rmcp_case(relay_url: &str) -> Result<()> {
    println!("[relay-rmcp] start (rmcp server + rmcp client)");

    let server_keys = signer::generate();
    let server_pubkey_hex = server_keys.public_key().to_hex();

    println!("[relay-rmcp] stage: spawning rmcp server task");
    let relay_url_owned = relay_url.to_string();
    let server_task = tokio::spawn(async move {
        let server = NostrMCPGateway::serve_handler(
            server_keys,
            server_config(&relay_url_owned),
            DemoServer::new(),
        )
        .await
        .with_context(|| format!("failed to start rmcp server on relay {relay_url_owned}"))?;

        let _ = server
            .waiting()
            .await
            .map_err(|e| anyhow!("rmcp server exited with error: {e}"))?;

        Err(anyhow!("rmcp server stopped unexpectedly"))
    });

    sleep(RELAY_WARMUP).await;

    if server_task.is_finished() {
        let res = server_task
            .await
            .map_err(|e| anyhow!("rmcp server task join error: {e}"))?;
        return res.context("rmcp server task ended before rmcp client startup");
    }

    let outcome: Result<()> = async {
        println!("[relay-rmcp] stage: starting rmcp relay client worker");

        let client = timeout(
            STARTUP_TIMEOUT,
            NostrMCPProxy::serve_client_handler(
                signer::generate(),
                client_config(relay_url, server_pubkey_hex),
                RelayRmcpClient,
            ),
        )
        .await
        .with_context(|| {
            format!(
                "timed out starting rmcp relay client worker after {:?}",
                STARTUP_TIMEOUT
            )
        })?
        .context("failed to start rmcp relay client")?;
        println!("[relay-rmcp] stage: rmcp relay client started");

        let peer = client
            .peer_info()
            .ok_or_else(|| anyhow!("rmcp relay client did not receive peer info"))?;
        let negotiated = peer.protocol_version.to_string();
        assert!(
            is_supported_protocol(&negotiated),
            "unexpected negotiated protocol version: {negotiated}"
        );

        let tools = client.list_all_tools().await?;
        assert!(
            tools.iter().any(|t| t.name == "echo"),
            "expected echo tool in rmcp relay case"
        );

        let echo = client
            .call_tool(call_params(
                "echo",
                Some(serde_json::json!({ "message": "rmcp-relay-hello" })),
            ))
            .await?;
        let echo_text = first_text(&echo);
        assert!(
            echo_text.contains("rmcp-relay-hello"),
            "unexpected rmcp relay echo output: {echo_text}"
        );

        let resources = client.list_all_resources().await?;
        assert!(
            resources.iter().any(|r| r.uri.as_str() == "demo://readme"),
            "expected demo://readme resource in rmcp relay case"
        );

        match client.call_tool(call_params("no_such_tool", None)).await {
            Err(_) => {}
            Ok(r) if r.is_error.unwrap_or(false) => {}
            Ok(_) => bail!("expected unknown tool to fail in rmcp relay case"),
        }

        client
            .cancel()
            .await
            .context("failed to cancel rmcp relay client")?;

        Ok(())
    }
    .await;

    server_task.abort();

    if server_task.is_finished() {
        let _ = server_task.await;
    }

    outcome?;

    println!("[relay-rmcp] pass");
    Ok(())
}

fn server_config(relay_url: &str) -> GatewayConfig {
    GatewayConfig {
        nostr_config: NostrServerTransportConfig {
            relay_urls: vec![relay_url.to_string()],
            encryption_mode: EncryptionMode::Optional,
            server_info: Some(CtxServerInfo {
                name: Some("rmcp-matrix-server".to_string()),
                about: Some("rmcp matrix coverage server".to_string()),
                ..Default::default()
            }),
            is_announced_server: false,
            ..Default::default()
        },
    }
}

fn client_config(relay_url: &str, server_pubkey: String) -> ProxyConfig {
    ProxyConfig {
        nostr_config: NostrClientTransportConfig {
            relay_urls: vec![relay_url.to_string()],
            server_pubkey,
            encryption_mode: EncryptionMode::Optional,
            ..Default::default()
        },
    }
}

async fn send_legacy_request_and_wait(
    proxy: &NostrMCPProxy,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    request: JsonRpcMessage,
    expected_id: &serde_json::Value,
) -> Result<JsonRpcMessage> {
    proxy.send(&request).await?;

    loop {
        let maybe_msg = timeout(IO_TIMEOUT, rx.recv())
            .await
            .context("timed out waiting for legacy response")?;

        let msg = maybe_msg.ok_or_else(|| anyhow!("legacy response channel closed"))?;

        if msg.id() == Some(expected_id) {
            return Ok(msg);
        }

        if msg.is_notification() {
            continue;
        }
    }
}

fn extract_tools_list(response: &JsonRpcMessage) -> Result<&Vec<serde_json::Value>> {
    let JsonRpcMessage::Response(resp) = response else {
        bail!("expected tools/list response, got {response:?}");
    };

    resp.result
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("tools/list response missing tools array"))
}

fn extract_first_content_text(response: &JsonRpcMessage) -> Result<String> {
    let JsonRpcMessage::Response(resp) = response else {
        bail!("expected tools/call response, got {response:?}");
    };

    let text = resp
        .result
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|text| text.as_str())
        .ok_or_else(|| anyhow!("tools/call response missing content[0].text"))?;

    Ok(text.to_string())
}

fn assert_initialize_shape(response: &JsonRpcMessage) -> Result<()> {
    let JsonRpcMessage::Response(resp) = response else {
        bail!("expected initialize response, got {response:?}");
    };
    let expected_protocol = mcp_protocol_version();
    let protocol = resp
        .result
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("initialize response missing protocolVersion"))?;

    if !is_supported_protocol(protocol) {
        bail!(
            "unexpected protocolVersion in initialize response: expected one of [{expected_protocol}, {}], got {protocol}",
            ProtocolVersion::LATEST
        );
    }

    if resp.result.get("serverInfo").is_none() {
        bail!("initialize response missing serverInfo");
    }

    Ok(())
}

fn is_supported_protocol(protocol: &str) -> bool {
    protocol == mcp_protocol_version() || protocol == ProtocolVersion::LATEST.to_string()
}

fn assert_error_response(response: &JsonRpcMessage) -> Result<()> {
    match response {
        JsonRpcMessage::ErrorResponse(err) => {
            if err.error.code >= 0 {
                bail!(
                    "expected negative JSON-RPC error code, got {}",
                    err.error.code
                );
            }
            Ok(())
        }
        JsonRpcMessage::Response(resp) => {
            if resp.result.get("isError") == Some(&serde_json::json!(true)) {
                Ok(())
            } else {
                bail!("expected error response but received success result")
            }
        }
        _ => bail!("expected error response, got {response:?}"),
    }
}

fn call_params(name: &'static str, args: Option<serde_json::Value>) -> CallToolRequestParams {
    CallToolRequestParams {
        name: name.into(),
        arguments: args.and_then(|v| serde_json::from_value(v).ok()),
        meta: None,
        task: None,
    }
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|content| {
            if let RawContent::Text(t) = &content.raw {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}
