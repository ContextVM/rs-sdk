//! Comprehensive rmcp integration matrix for ContextVM SDK.
//!
//! This example validates three scenarios:
//! 1) local rmcp transport (in-process duplex)
//! 2) hybrid relay mode (rmcp server + legacy JSON-RPC client)
//! 3) full rmcp over relays (rmcp server + rmcp client)
//! 4) CEP-19 response mode matrix (persistent/ephemeral/optional combinations)
//! 5) CEP-19 optional-mode learning upgrade path
//! 6) CEP-19 notification mode matrix
//!
//! Run:
//!   cargo run --example rmcp_integration_test --features rmcp
//!   cargo run --example rmcp_integration_test --features rmcp -- local
//!   cargo run --example rmcp_integration_test --features rmcp -- hybrid
//!   cargo run --example rmcp_integration_test --features rmcp -- relay-rmcp
//!   cargo run --example rmcp_integration_test --features rmcp -- cep19-response-matrix
//!   cargo run --example rmcp_integration_test --features rmcp -- cep19-learning
//!   cargo run --example rmcp_integration_test --features rmcp -- cep19-notification-matrix
//!   cargo run --example rmcp_integration_test --features rmcp -- cep19-all
//!   cargo run --example rmcp_integration_test --features rmcp -- all
//!
//! Optional relay override:
//!   CTXVM_RELAY_URL=wss://relay.primal.net cargo run --example rmcp_integration_test --features rmcp -- all
//!   cargo run --example rmcp_integration_test --features rmcp -- all wss://relay.primal.net

use anyhow::{anyhow, bail, Context, Result};
use contextvm_sdk::core::constants::mcp_protocol_version;
use contextvm_sdk::core::types::{
    EncryptionMode, GiftWrapMode, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, ServerInfo as CtxServerInfo,
};
use contextvm_sdk::gateway::{GatewayConfig, NostrMCPGateway};
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{
    IncomingRequest, NostrServerTransport, NostrServerTransportConfig,
};
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
const CEP19_NEGATIVE_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Local,
    Hybrid,
    RelayRmcp,
    Cep19ResponseMatrix,
    Cep19Learning,
    Cep19NotificationMatrix,
    Cep19All,
    All,
}

impl Mode {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("all") {
            "local" => Ok(Self::Local),
            "hybrid" => Ok(Self::Hybrid),
            "relay-rmcp" => Ok(Self::RelayRmcp),
            "cep19-response-matrix" => Ok(Self::Cep19ResponseMatrix),
            "cep19-learning" => Ok(Self::Cep19Learning),
            "cep19-notification-matrix" => Ok(Self::Cep19NotificationMatrix),
            "cep19-all" => Ok(Self::Cep19All),
            "all" => Ok(Self::All),
            other => bail!(
                "Unknown mode '{other}'. Use one of: local | hybrid | relay-rmcp | cep19-response-matrix | cep19-learning | cep19-notification-matrix | cep19-all | all"
            ),
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

    fn run_cep19_response_matrix(self) -> bool {
        matches!(self, Self::Cep19ResponseMatrix | Self::Cep19All | Self::All)
    }

    fn run_cep19_learning(self) -> bool {
        matches!(self, Self::Cep19Learning | Self::Cep19All | Self::All)
    }

    fn run_cep19_notification_matrix(self) -> bool {
        matches!(self, Self::Cep19NotificationMatrix | Self::Cep19All | Self::All)
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

    if mode.run_cep19_response_matrix() {
        run_cep19_response_matrix_case(&relay_url).await?;
    }

    if mode.run_cep19_learning() {
        run_cep19_learning_case(&relay_url).await?;
    }

    if mode.run_cep19_notification_matrix() {
        run_cep19_notification_matrix_case(&relay_url).await?;
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

#[derive(Clone, Copy)]
struct Cep19ResponseCase {
    name: &'static str,
    server_mode: GiftWrapMode,
    client_mode: GiftWrapMode,
    expect_success: bool,
}

#[derive(Clone, Copy)]
struct Cep19NotificationCase {
    name: &'static str,
    server_mode: GiftWrapMode,
    client_mode: GiftWrapMode,
    expect_notification: bool,
}

async fn run_cep19_response_matrix_case(relay_url: &str) -> Result<()> {
    println!("[cep19-response-matrix] start");

    let cases = [
        Cep19ResponseCase {
            name: "persistent-persistent",
            server_mode: GiftWrapMode::Persistent,
            client_mode: GiftWrapMode::Persistent,
            expect_success: true,
        },
        Cep19ResponseCase {
            name: "ephemeral-ephemeral",
            server_mode: GiftWrapMode::Ephemeral,
            client_mode: GiftWrapMode::Ephemeral,
            expect_success: true,
        },
        Cep19ResponseCase {
            name: "persistent-server-ephemeral-client",
            server_mode: GiftWrapMode::Persistent,
            client_mode: GiftWrapMode::Ephemeral,
            expect_success: false,
        },
        Cep19ResponseCase {
            name: "ephemeral-server-persistent-client",
            server_mode: GiftWrapMode::Ephemeral,
            client_mode: GiftWrapMode::Persistent,
            expect_success: false,
        },
        Cep19ResponseCase {
            name: "optional-server-persistent-client",
            server_mode: GiftWrapMode::Optional,
            client_mode: GiftWrapMode::Persistent,
            expect_success: true,
        },
        Cep19ResponseCase {
            name: "optional-server-ephemeral-client",
            server_mode: GiftWrapMode::Optional,
            client_mode: GiftWrapMode::Ephemeral,
            expect_success: true,
        },
    ];

    for case in cases {
        run_single_cep19_response_case(relay_url, case).await?;
    }

    println!("[cep19-response-matrix] pass");
    Ok(())
}

async fn run_single_cep19_response_case(relay_url: &str, case: Cep19ResponseCase) -> Result<()> {
    println!(
        "[cep19-response-matrix] case={} server_mode={:?} client_mode={:?}",
        case.name, case.server_mode, case.client_mode
    );

    let server_keys = signer::generate();
    let server_pubkey_hex = server_keys.public_key().to_hex();

    let relay_url_owned = relay_url.to_string();
    let server_mode = case.server_mode;
    let server_task = tokio::spawn(async move {
        let server = NostrMCPGateway::serve_handler(
            server_keys,
            server_config_with_modes(&relay_url_owned, EncryptionMode::Optional, server_mode),
            DemoServer::new(),
        )
        .await
        .with_context(|| format!("failed to start CEP-19 matrix server on relay {relay_url_owned}"))?;

        let _ = server
            .waiting()
            .await
            .map_err(|e| anyhow!("CEP-19 matrix server exited with error: {e}"))?;

        Err(anyhow!("CEP-19 matrix server stopped unexpectedly"))
    });

    sleep(RELAY_WARMUP).await;

    if server_task.is_finished() {
        let res = server_task
            .await
            .map_err(|e| anyhow!("CEP-19 matrix server task join error: {e}"))?;
        return res.context("CEP-19 matrix server ended before client startup");
    }

    let outcome: Result<()> = async {
        let mut proxy = timeout(
            STARTUP_TIMEOUT,
            NostrMCPProxy::new(
                signer::generate(),
                client_config_with_modes(
                    relay_url,
                    server_pubkey_hex.clone(),
                    EncryptionMode::Optional,
                    case.client_mode,
                ),
            ),
        )
        .await
        .with_context(|| {
            format!(
                "timed out creating CEP-19 matrix proxy client after {:?}",
                STARTUP_TIMEOUT
            )
        })?
        .context("failed to create CEP-19 matrix proxy client")?;

        let mut rx = timeout(STARTUP_TIMEOUT, proxy.start())
            .await
            .with_context(|| {
                format!(
                    "timed out starting CEP-19 matrix proxy transport after {:?}",
                    STARTUP_TIMEOUT
                )
            })?
            .context("failed to start CEP-19 matrix proxy")?;

        let init_id = serde_json::json!(format!("cep19-response-init-{}", case.name));
        let init_request = initialize_request(init_id.clone(), "cep19-response-matrix-client");

        if case.expect_success {
            let init_response =
                send_legacy_request_and_wait(&proxy, &mut rx, init_request, &init_id).await?;
            assert_initialize_shape(&init_response)?;

            proxy
                .send(&initialized_notification())
                .await
                .context("failed to send initialized notification")?;

            let tools_id = serde_json::json!(format!("cep19-response-tools-{}", case.name));
            let tools_response = send_legacy_request_and_wait(
                &proxy,
                &mut rx,
                tools_list_request(tools_id.clone()),
                &tools_id,
            )
            .await?;
            let _ = extract_tools_list(&tools_response)?;
        } else {
            proxy
                .send(&init_request)
                .await
                .context("failed to send initialize request in negative CEP-19 case")?;
            assert_no_response_for_id(&mut rx, &init_id, CEP19_NEGATIVE_WAIT, case.name).await?;
        }

        proxy.stop().await.context("failed to stop CEP-19 matrix proxy")?;
        Ok(())
    }
    .await;

    server_task.abort();
    if server_task.is_finished() {
        let _ = server_task.await;
    }

    outcome.with_context(|| format!("CEP-19 response matrix case '{}' failed", case.name))
}

async fn run_cep19_learning_case(relay_url: &str) -> Result<()> {
    println!("[cep19-learning] start");

    let server_keys = signer::generate();
    let server_pubkey_hex = server_keys.public_key().to_hex();

    let mut server_phase1 = NostrServerTransport::new(
        server_keys.clone(),
        server_transport_config_with_modes(
            relay_url,
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
        ),
    )
    .await?;
    server_phase1.start().await?;
    let mut server_phase1_rx = server_phase1
        .take_message_receiver()
        .ok_or_else(|| anyhow!("failed to acquire server phase 1 receiver"))?;

    let mut client = NostrClientTransport::new(
        signer::generate(),
        client_transport_config_with_modes(
            relay_url,
            server_pubkey_hex.clone(),
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
        ),
    )
    .await?;
    client.start().await?;
    let mut client_rx = client
        .take_message_receiver()
        .ok_or_else(|| anyhow!("failed to acquire client receiver in CEP-19 learning case"))?;

    let init_id = serde_json::json!("cep19-learning-init");
    client
        .send(&initialize_request(init_id.clone(), "cep19-learning-client"))
        .await?;
    let incoming_init = receive_server_request(
        &mut server_phase1_rx,
        "CEP-19 learning phase 1 initialize request",
        "initialize",
    )
    .await?;
    server_phase1
        .send_response(&incoming_init.event_id, initialize_result_response())
        .await?;

    let init_response = wait_for_client_message_with_id(
        &mut client_rx,
        &init_id,
        IO_TIMEOUT,
        "CEP-19 learning phase 1 initialize response",
    )
    .await?;
    assert_initialize_shape(&init_response)?;

    if !client.server_supports_ephemeral_encryption() {
        bail!(
            "CEP-19 learning case failed: optional-mode client did not learn server ephemeral support"
        );
    }

    server_phase1.close().await?;
    sleep(RELAY_WARMUP).await;

    let mut server_phase2 = NostrServerTransport::new(
        server_keys,
        server_transport_config_with_modes(
            relay_url,
            EncryptionMode::Optional,
            GiftWrapMode::Ephemeral,
        ),
    )
    .await?;
    server_phase2.start().await?;
    let mut server_phase2_rx = server_phase2
        .take_message_receiver()
        .ok_or_else(|| anyhow!("failed to acquire server phase 2 receiver"))?;

    let tools_id = serde_json::json!("cep19-learning-tools");
    client.send(&tools_list_request(tools_id.clone())).await?;

    let incoming_tools = receive_server_request(
        &mut server_phase2_rx,
        "CEP-19 learning phase 2 tools/list request",
        "tools/list",
    )
    .await?;
    server_phase2
        .send_response(&incoming_tools.event_id, tools_list_response())
        .await?;

    let tools_response = wait_for_client_message_with_id(
        &mut client_rx,
        &tools_id,
        IO_TIMEOUT,
        "CEP-19 learning phase 2 tools/list response",
    )
    .await?;
    let _ = extract_tools_list(&tools_response)?;

    server_phase2.close().await?;
    client.close().await?;

    println!("[cep19-learning] pass");
    Ok(())
}

async fn run_cep19_notification_matrix_case(relay_url: &str) -> Result<()> {
    println!("[cep19-notification-matrix] start");

    let cases = [
        Cep19NotificationCase {
            name: "optional-server-ephemeral-client",
            server_mode: GiftWrapMode::Optional,
            client_mode: GiftWrapMode::Ephemeral,
            expect_notification: false,
        },
        Cep19NotificationCase {
            name: "optional-server-persistent-client",
            server_mode: GiftWrapMode::Optional,
            client_mode: GiftWrapMode::Persistent,
            expect_notification: true,
        },
        Cep19NotificationCase {
            name: "ephemeral-server-ephemeral-client",
            server_mode: GiftWrapMode::Ephemeral,
            client_mode: GiftWrapMode::Ephemeral,
            expect_notification: true,
        },
        Cep19NotificationCase {
            name: "persistent-server-persistent-client",
            server_mode: GiftWrapMode::Persistent,
            client_mode: GiftWrapMode::Persistent,
            expect_notification: true,
        },
    ];

    for case in cases {
        run_single_cep19_notification_case(relay_url, case).await?;
    }

    println!("[cep19-notification-matrix] pass");
    Ok(())
}

async fn run_single_cep19_notification_case(
    relay_url: &str,
    case: Cep19NotificationCase,
) -> Result<()> {
    println!(
        "[cep19-notification-matrix] case={} server_mode={:?} client_mode={:?}",
        case.name, case.server_mode, case.client_mode
    );

    let server_keys = signer::generate();
    let server_pubkey_hex = server_keys.public_key().to_hex();

    let mut server = NostrServerTransport::new(
        server_keys,
        server_transport_config_with_modes(
            relay_url,
            EncryptionMode::Optional,
            case.server_mode,
        ),
    )
    .await?;
    server.start().await?;
    let mut server_rx = server
        .take_message_receiver()
        .ok_or_else(|| anyhow!("failed to acquire server receiver in CEP-19 notification case"))?;

    let mut client = NostrClientTransport::new(
        signer::generate(),
        client_transport_config_with_modes(
            relay_url,
            server_pubkey_hex,
            EncryptionMode::Optional,
            case.client_mode,
        ),
    )
    .await?;
    client.start().await?;
    let mut client_rx = client.take_message_receiver().ok_or_else(|| {
        anyhow!("failed to acquire client receiver in CEP-19 notification case")
    })?;

    let init_id = serde_json::json!(format!("cep19-notif-init-{}", case.name));
    client
        .send(&initialize_request(
            init_id.clone(),
            "cep19-notification-matrix-client",
        ))
        .await?;
    let incoming_init =
        receive_server_request(
            &mut server_rx,
            "CEP-19 notification matrix initialize request",
            "initialize",
        )
        .await?;
    server
        .send_response(&incoming_init.event_id, initialize_result_response())
        .await?;
    let init_response = wait_for_client_message_with_id(
        &mut client_rx,
        &init_id,
        IO_TIMEOUT,
        "CEP-19 notification matrix initialize response",
    )
    .await?;
    assert_initialize_shape(&init_response)?;

    client
        .send(&initialized_notification())
        .await
        .context("failed to send initialized notification in CEP-19 notification matrix")?;

    let tools_id = serde_json::json!(format!("cep19-notif-tools-{}", case.name));
    client.send(&tools_list_request(tools_id.clone())).await?;
    let incoming_tools =
        receive_server_request(
            &mut server_rx,
            "CEP-19 notification matrix tools/list request",
            "tools/list",
        )
        .await?;

    server
        .send_notification(
            &incoming_tools.client_pubkey,
            &payment_required_notification(),
            Some(&incoming_tools.event_id),
        )
        .await?;
    server
        .send_response(&incoming_tools.event_id, tools_list_response())
        .await?;

    let (tools_response, mut saw_notification) = wait_for_response_and_track_notification(
        &mut client_rx,
        &tools_id,
        "notifications/payment_required",
        IO_TIMEOUT,
    )
    .await?;
    let _ = extract_tools_list(&tools_response)?;

    if case.expect_notification && !saw_notification {
        saw_notification = wait_for_notification_method(
            &mut client_rx,
            "notifications/payment_required",
            Duration::from_secs(5),
        )
        .await?;
    }

    if saw_notification != case.expect_notification {
        bail!(
            "CEP-19 notification matrix case '{}' expected notification_seen={} but got {}",
            case.name,
            case.expect_notification,
            saw_notification
        );
    }

    server.close().await?;
    client.close().await?;

    println!("[cep19-notification-matrix] case={} pass", case.name);
    Ok(())
}

fn server_config(relay_url: &str) -> GatewayConfig {
    server_config_with_modes(
        relay_url,
        EncryptionMode::Optional,
        GiftWrapMode::Optional,
    )
}

fn server_config_with_modes(
    relay_url: &str,
    encryption_mode: EncryptionMode,
    gift_wrap_mode: GiftWrapMode,
) -> GatewayConfig {
    GatewayConfig {
        nostr_config: NostrServerTransportConfig {
            relay_urls: vec![relay_url.to_string()],
            encryption_mode,
            gift_wrap_mode,
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
    client_config_with_modes(
        relay_url,
        server_pubkey,
        EncryptionMode::Optional,
        GiftWrapMode::Optional,
    )
}

fn client_config_with_modes(
    relay_url: &str,
    server_pubkey: String,
    encryption_mode: EncryptionMode,
    gift_wrap_mode: GiftWrapMode,
) -> ProxyConfig {
    ProxyConfig {
        nostr_config: NostrClientTransportConfig {
            relay_urls: vec![relay_url.to_string()],
            server_pubkey,
            encryption_mode,
            gift_wrap_mode,
            ..Default::default()
        },
    }
}

fn server_transport_config_with_modes(
    relay_url: &str,
    encryption_mode: EncryptionMode,
    gift_wrap_mode: GiftWrapMode,
) -> NostrServerTransportConfig {
    NostrServerTransportConfig {
        relay_urls: vec![relay_url.to_string()],
        encryption_mode,
        gift_wrap_mode,
        server_info: Some(CtxServerInfo {
            name: Some("cep19-transport-server".to_string()),
            about: Some("CEP-19 transport matrix server".to_string()),
            ..Default::default()
        }),
        is_announced_server: false,
        ..Default::default()
    }
}

fn client_transport_config_with_modes(
    relay_url: &str,
    server_pubkey: String,
    encryption_mode: EncryptionMode,
    gift_wrap_mode: GiftWrapMode,
) -> NostrClientTransportConfig {
    NostrClientTransportConfig {
        relay_urls: vec![relay_url.to_string()],
        server_pubkey,
        encryption_mode,
        gift_wrap_mode,
        ..Default::default()
    }
}

fn initialize_request(id: serde_json::Value, client_name: &str) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "clientInfo": {
                "name": client_name,
                "version": "0.1.0"
            }
        })),
    })
}

fn tools_list_request(id: serde_json::Value) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "tools/list".to_string(),
        params: Some(serde_json::json!({})),
    })
}

fn initialized_notification() -> JsonRpcMessage {
    JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/initialized".to_string(),
        params: None,
    })
}

fn payment_required_notification() -> JsonRpcMessage {
    JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/payment_required".to_string(),
        params: Some(serde_json::json!({
            "reason": "cep19-matrix-check"
        })),
    })
}

fn initialize_result_response() -> JsonRpcMessage {
    JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(0),
        result: serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "serverInfo": {
                "name": "cep19-matrix-server",
                "version": "0.1.0"
            },
            "capabilities": {
                "tools": {},
                "resources": {}
            }
        }),
    })
}

fn tools_list_response() -> JsonRpcMessage {
    JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(0),
        result: serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "Echo tool for CEP-19 matrix testing"
            }]
        }),
    })
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

async fn assert_no_response_for_id(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    expected_id: &serde_json::Value,
    wait_for: Duration,
    case_name: &str,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + wait_for;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(());
        }

        let remaining = deadline.saturating_duration_since(now);
        match timeout(remaining, rx.recv()).await {
            Ok(Some(msg)) => {
                if msg.id() == Some(expected_id) {
                    bail!(
                        "CEP-19 case '{case_name}' unexpectedly received response for id {expected_id}"
                    );
                }
            }
            Ok(None) => return Ok(()),
            Err(_) => return Ok(()),
        }
    }
}

async fn receive_server_request(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
    stage: &str,
    expected_method: &str,
) -> Result<IncomingRequest> {
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!(
                "timed out waiting for server request method '{}' during {}",
                expected_method,
                stage
            );
        }

        let remaining = deadline.saturating_duration_since(now);
        let maybe_msg = timeout(remaining, rx.recv())
            .await
            .with_context(|| format!("timed out waiting for server request during {stage}"))?;
        let msg =
            maybe_msg.ok_or_else(|| anyhow!("server request channel closed during {stage}"))?;

        if msg.message.is_request() && msg.message.method() == Some(expected_method) {
            return Ok(msg);
        }
    }
}

async fn wait_for_client_message_with_id(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    expected_id: &serde_json::Value,
    timeout_duration: Duration,
    stage: &str,
) -> Result<JsonRpcMessage> {
    let deadline = tokio::time::Instant::now() + timeout_duration;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!(
                "timed out waiting for client message id {} during {}",
                expected_id,
                stage
            );
        }

        let remaining = deadline.saturating_duration_since(now);
        let maybe_msg = timeout(remaining, rx.recv())
            .await
            .with_context(|| format!("timed out waiting for client message during {stage}"))?;

        let msg = maybe_msg.ok_or_else(|| anyhow!("client response channel closed during {stage}"))?;

        if msg.id() == Some(expected_id) {
            return Ok(msg);
        }
    }
}

async fn wait_for_response_and_track_notification(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    expected_id: &serde_json::Value,
    notification_method: &str,
    timeout_duration: Duration,
) -> Result<(JsonRpcMessage, bool)> {
    let deadline = tokio::time::Instant::now() + timeout_duration;
    let mut saw_notification = false;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!(
                "timed out waiting for response id {} while tracking notification {}",
                expected_id,
                notification_method
            );
        }

        let remaining = deadline.saturating_duration_since(now);
        let maybe_msg = timeout(remaining, rx.recv())
            .await
            .context("timed out while waiting for tracked response")?;
        let msg = maybe_msg.ok_or_else(|| anyhow!("client channel closed while tracking response"))?;

        if msg.method() == Some(notification_method) {
            saw_notification = true;
        }

        if msg.id() == Some(expected_id) {
            return Ok((msg, saw_notification));
        }
    }
}

async fn wait_for_notification_method(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    notification_method: &str,
    timeout_duration: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout_duration;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(false);
        }

        let remaining = deadline.saturating_duration_since(now);
        match timeout(remaining, rx.recv()).await {
            Ok(Some(msg)) => {
                if msg.method() == Some(notification_method) {
                    return Ok(true);
                }
            }
            Ok(None) => return Ok(false),
            Err(_) => return Ok(false),
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
