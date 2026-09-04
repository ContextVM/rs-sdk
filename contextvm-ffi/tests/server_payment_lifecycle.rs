//! End-to-end payment lifecycle tests for the UniFFI `Server` object.
//!
//! These tests drive the two-phase `Server` lifecycle through a shared
//! `MockRelayPool` and verify CEP-8 payment-interaction negotiation:
//!
//! - `Optional` server accepts a client's `explicit_gating` request and the
//!   client learns the negotiated mode.
//! - `Transparent` server rejects `explicit_gating` with a `-32602` JSON-RPC
//!   error instead of forwarding the request.
//!
//! The tests avoid live network. `Server` operations are synchronous FFI calls
//! that block on the global runtime; SDK client operations are driven through
//! the same runtime using `global_runtime().block_on`.

use contextvm_ffi::{
    global_runtime, EncryptionMode, ErrorCode, Keys, PaymentInteractionPolicy, Server, ServerConfig,
};
use contextvm_sdk as sdk;
use std::sync::Arc;
use std::time::Duration;

fn request(id: &str, method: &str) -> sdk::JsonRpcMessage {
    sdk::JsonRpcMessage::Request(sdk::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: method.to_string(),
        params: None,
    })
}

fn response(id: serde_json::Value) -> String {
    let msg = sdk::JsonRpcMessage::Response(sdk::JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: serde_json::json!({"tools": []}),
    });
    serde_json::to_string(&msg).unwrap()
}

fn make_server_with_pool(
    keys: &Keys,
    config: &ServerConfig,
    pool: Arc<sdk::relay::MockRelayPool>,
) -> Server {
    Server::new_with_relay_pool(keys, config, pool).expect("create server with mock relay pool")
}

fn fresh_fixture() -> (
    Keys,
    sdk::signer::Keys,
    Arc<sdk::relay::MockRelayPool>,
    Arc<sdk::relay::MockRelayPool>,
) {
    let server_sdk_keys = sdk::signer::generate();
    let server_keys = Keys::from_secret_key(&server_sdk_keys.secret_key().to_secret_hex()).unwrap();
    let (client_pool, server_pool) = sdk::relay::MockRelayPool::create_pair();
    let client_pool = Arc::new(client_pool);
    let server_pool = Arc::new(server_pool.linked_with_keys(server_sdk_keys.clone()));
    (server_keys, server_sdk_keys, client_pool, server_pool)
}

#[test]
fn optional_server_negotiates_explicit_gating() {
    let (server_keys, server_sdk_keys, client_pool, server_pool) = fresh_fixture();

    let server_config = ServerConfig {
        encryption_mode: EncryptionMode::Disabled,
        ..Default::default()
    };
    let server = make_server_with_pool(&server_keys, &server_config, server_pool);

    server
        .set_payment_interaction_policy(PaymentInteractionPolicy::Optional)
        .expect("set optional policy before start");
    server
        .set_priced_capabilities_json(
            r#"[{"method":"tools/list","amount":1000,"currencyUnit":"sats"}]"#,
        )
        .expect("set priced capabilities before start");
    server.start().expect("start server");

    let client_config = sdk::NostrClientTransportConfig::default()
        .with_relay_urls(vec!["wss://mock.relay".to_string()])
        .with_server_pubkey(server_sdk_keys.public_key().to_hex())
        .with_encryption_mode(sdk::EncryptionMode::Disabled)
        .with_pmis(vec!["bitcoin-lightning-bolt11".to_string()])
        .with_payment_interaction(sdk::PaymentInteractionMode::ExplicitGating);

    let (mut client, mut client_rx) = global_runtime().block_on(async {
        let mut client = sdk::NostrClientTransport::with_relay_pool(client_config, client_pool)
            .await
            .expect("create client transport");
        client.start().await.expect("start client");
        let client_rx = client.take_message_receiver().expect("client receiver");

        // Give the server event loop a moment to subscribe before sending.
        tokio::time::sleep(Duration::from_millis(50)).await;

        client
            .send(&request("e2e-1", "tools/list"))
            .await
            .expect("send client request");

        // Wait for the mock relay to deliver the request to the server event loop.
        tokio::time::sleep(Duration::from_millis(50)).await;

        (client, client_rx)
    });

    // The server must receive the request because Optional accepts explicit_gating.
    let incoming = server.recv_timeout(2).expect("server receives the request");
    let request_id = incoming.message.id.clone();
    let event_id = incoming.event_id;

    server
        .send_response(&event_id, &response(serde_json::json!(request_id)))
        .expect("send response");

    let msg = global_runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(2), client_rx.recv())
            .await
            .expect("client receives a response")
            .expect("client channel is open")
    });

    let json = serde_json::to_string(&msg).unwrap();
    assert!(
        json.contains("tools"),
        "client should receive a successful response: {json}"
    );

    assert_eq!(
        client.get_effective_payment_interaction(),
        Some(sdk::PaymentInteractionMode::ExplicitGating),
        "client must learn the negotiated explicit-gating mode"
    );

    server.close().expect("close server");
    let _ = global_runtime().block_on(client.close());
}

#[test]
fn transparent_server_rejects_explicit_gating() {
    let (server_keys, server_sdk_keys, client_pool, server_pool) = fresh_fixture();

    let server_config = ServerConfig {
        encryption_mode: EncryptionMode::Disabled,
        ..Default::default()
    };
    let server = make_server_with_pool(&server_keys, &server_config, server_pool);

    server
        .set_payment_interaction_policy(PaymentInteractionPolicy::Transparent)
        .expect("set transparent policy before start");
    server.start().expect("start server");

    let client_config = sdk::NostrClientTransportConfig::default()
        .with_relay_urls(vec!["wss://mock.relay".to_string()])
        .with_server_pubkey(server_sdk_keys.public_key().to_hex())
        .with_encryption_mode(sdk::EncryptionMode::Disabled)
        .with_payment_interaction(sdk::PaymentInteractionMode::ExplicitGating);

    let (mut client, mut client_rx) = global_runtime().block_on(async {
        let mut client = sdk::NostrClientTransport::with_relay_pool(client_config, client_pool)
            .await
            .expect("create client transport");
        client.start().await.expect("start client");
        let client_rx = client.take_message_receiver().expect("client receiver");

        tokio::time::sleep(Duration::from_millis(50)).await;

        client
            .send(&request("reject-1", "tools/list"))
            .await
            .expect("send client request");

        tokio::time::sleep(Duration::from_millis(50)).await;

        (client, client_rx)
    });

    // The transparent server should reject the request and never forward it.
    let err = server
        .recv_timeout(2)
        .expect_err("server must not receive a request");
    assert_eq!(err.code, ErrorCode::Timeout);

    let msg = global_runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(2), client_rx.recv())
            .await
            .expect("client receives a response")
            .expect("client channel is open")
    });

    let json = serde_json::to_string(&msg).unwrap();
    assert!(
        json.contains("-32602"),
        "client should receive unsupported payment_interaction error: {json}"
    );

    server.close().expect("close server");
    let _ = global_runtime().block_on(client.close());
}
