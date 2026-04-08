//! Local RMCP integration test (in-process duplex I/O, no relay required).
//! Relay-dependent scenarios live in `examples/rmcp_integration_test.rs`
//! and run via the `integration.yml` workflow against a local relay container.

#![cfg(feature = "rmcp")]

use rmcp::{
    handler::server::router::tool::ToolRouter, handler::server::wrapper::Parameters, model::*,
    schemars, service::RequestContext, tool, tool_handler, tool_router, ClientHandler, RoleServer,
    ServerHandler, ServiceExt,
};
use std::sync::Arc;
use tokio::sync::Mutex;

// Minimal fixture: same tools as examples/rmcp_integration_test.rs

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
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "integration-test".to_string(),
                title: None,
                version: "0.1.0".to_string(),
                description: None,
                icons: None,
                website_url: None,
            },
            instructions: None,
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
                contents: vec![ResourceContents::text("Demo content.", req.uri)],
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

// ── Test ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_local_rmcp() {
    let (server_io, client_io) = tokio::io::duplex(65536);

    let server_handle = tokio::spawn(async move {
        DemoServer::new()
            .serve(server_io)
            .await
            .expect("serve")
            .waiting()
            .await
            .expect("server error");
    });

    let client = DemoClient.serve(client_io).await.expect("client init");

    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(tools.len(), 3);

    let add = client
        .call_tool(CallToolRequestParams {
            name: "add".into(),
            arguments: serde_json::from_value(serde_json::json!({ "a": 7, "b": 5 })).ok(),
            meta: None,
            task: None,
        })
        .await
        .expect("call add");
    assert!(first_text(&add).contains("12"));

    let resources = client.list_all_resources().await.expect("list resources");
    assert_eq!(resources.len(), 1);

    match client
        .call_tool(CallToolRequestParams {
            name: "no_such_tool".into(),
            arguments: None,
            meta: None,
            task: None,
        })
        .await
    {
        Err(_) => {}
        Ok(r) if r.is_error.unwrap_or(false) => {}
        Ok(_) => panic!("expected unknown tool to fail"),
    }

    client.cancel().await.expect("cancel");
    server_handle.abort();
}
