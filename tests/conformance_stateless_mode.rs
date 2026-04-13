//! Stateless-mode conformance tests for the client transport.

use std::time::Duration;

use contextvm_sdk::core::constants::{mcp_protocol_version, INITIALIZE_METHOD};
use contextvm_sdk::core::types::{EncryptionMode, JsonRpcMessage, JsonRpcRequest};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use tokio::time::timeout;

async fn make_stateless_transport() -> (
    NostrClientTransport,
    tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
) {
    let server_keys = signer::generate();
    let client_keys = signer::generate();

    let config = NostrClientTransportConfig {
        relay_urls: Vec::new(),
        server_pubkey: server_keys.public_key().to_hex(),
        encryption_mode: EncryptionMode::Optional,
        is_stateless: true,
        timeout: Duration::from_secs(1),
        seen_event_cache_size: 64,
        log_file_path: None,
    };

    let mut transport = NostrClientTransport::new(client_keys, config)
        .await
        .expect("transport should be constructed");
    let rx = transport
        .take_message_receiver()
        .expect("message receiver should be available once");

    (transport, rx)
}

#[tokio::test]
async fn create_emulated_response_returns_correct_request_id() {
    let (transport, mut rx) = make_stateless_transport().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("test-id"),
        method: INITIALIZE_METHOD.to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": { "name": "conformance-test", "version": "0.0.0" }
        })),
    });

    transport
        .send(&request)
        .await
        .expect("initialize should be emulated in stateless mode");

    let msg = timeout(Duration::from_millis(200), rx.recv())
        .await
        .expect("should receive emulated response promptly")
        .expect("channel should contain response");

    match msg {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.id, serde_json::json!("test-id"));
            assert_eq!(resp.jsonrpc, "2.0");
            assert_eq!(
                resp.result
                    .get("protocolVersion")
                    .and_then(serde_json::Value::as_str),
                Some(mcp_protocol_version())
            );
            assert_eq!(
                resp.result
                    .get("serverInfo")
                    .and_then(|v| v.get("name"))
                    .and_then(serde_json::Value::as_str),
                Some("Emulated-Stateless-Server")
            );
            assert_eq!(
                resp.result
                    .get("serverInfo")
                    .and_then(|v| v.get("version"))
                    .and_then(serde_json::Value::as_str),
                Some("1.0.0")
            );
            assert_eq!(
                resp.result
                    .get("capabilities")
                    .and_then(|v| v.get("tools"))
                    .and_then(|v| v.get("listChanged"))
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );
            assert_eq!(
                resp.result
                    .get("capabilities")
                    .and_then(|v| v.get("prompts"))
                    .and_then(|v| v.get("listChanged"))
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );
            assert_eq!(
                resp.result
                    .get("capabilities")
                    .and_then(|v| v.get("resources"))
                    .and_then(|v| v.get("subscribe"))
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );
            assert_eq!(
                resp.result
                    .get("capabilities")
                    .and_then(|v| v.get("resources"))
                    .and_then(|v| v.get("listChanged"))
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );
        }
        other => panic!("expected Response, got {other:?}"),
    }

    let duplicate = timeout(Duration::from_millis(100), rx.recv()).await;
    assert!(
        duplicate.is_err(),
        "initialize request should emit exactly one emulated response"
    );
}

#[tokio::test]
async fn should_handle_statelessly_returns_true_for_initialize() {
    let (transport, mut rx) = make_stateless_transport().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: INITIALIZE_METHOD.to_string(),
        params: None,
    });

    transport
        .send(&request)
        .await
        .expect("initialize should be handled statelessly");

    let msg = timeout(Duration::from_millis(200), rx.recv())
        .await
        .expect("initialize should produce local emulated response")
        .expect("response should be delivered");

    assert_eq!(msg.id(), Some(&serde_json::json!(1)));
}

#[tokio::test]
async fn should_handle_statelessly_returns_false_for_other_methods() {
    let (transport, mut rx) = make_stateless_transport().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(2),
        method: "tools/list".to_string(),
        params: None,
    });

    let _send_result = transport.send(&request).await;

    let recv_result = timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        recv_result.is_err(),
        "non-initialize request should not create a local emulated response"
    );
}
