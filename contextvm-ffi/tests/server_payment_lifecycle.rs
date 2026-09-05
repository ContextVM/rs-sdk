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

fn tools_call(id: &str, name: &str) -> sdk::JsonRpcMessage {
    sdk::JsonRpcMessage::Request(sdk::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({"name": name, "arguments": {}})),
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

#[test]
fn transparent_park_settle_and_deliver() {
    let (server_keys, server_sdk_keys, client_pool, server_pool) = fresh_fixture();

    let server_config = ServerConfig {
        encryption_mode: EncryptionMode::Disabled,
        ..Default::default()
    };
    let server = make_server_with_pool(&server_keys, &server_config, server_pool);

    server
        .set_payment_interaction_policy(PaymentInteractionPolicy::Transparent)
        .expect("set transparent policy");
    server
        .set_priced_capabilities_json(
            r#"[{"method":"tools/call","name":"download_media","amount":1000,"currencyUnit":"sats"}]"#,
        )
        .expect("set priced capabilities");
    server.start().expect("start server");

    let client_config = sdk::NostrClientTransportConfig::default()
        .with_relay_urls(vec!["wss://mock.relay".to_string()])
        .with_server_pubkey(server_sdk_keys.public_key().to_hex())
        .with_encryption_mode(sdk::EncryptionMode::Disabled)
        .with_pmis(vec!["bitcoin-lightning-bolt11".to_string()]);

    let (mut client, mut client_rx) = global_runtime().block_on(async {
        let mut client = sdk::NostrClientTransport::with_relay_pool(client_config, client_pool)
            .await
            .expect("create client");
        client.start().await.expect("start client");
        let client_rx = client.take_message_receiver().expect("client receiver");
        tokio::time::sleep(Duration::from_millis(50)).await;
        client
            .send(&tools_call("t1", "download_media"))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        (client, client_rx)
    });

    // The gate parks the paid request and emits a PaymentGateRequest.
    let event = server
        .recv_payment_gate_request(2)
        .expect("receive payment gate request")
        .expect("payment gate request present");
    assert_eq!(event.method, "tools/call");
    assert_eq!(event.capability_name, "download_media");

    // Submit an invoice. Client receives notifications/payment_required.
    server
        .submit_invoice(
            event.request_event_id.clone(),
            1000,
            "lnbc1transparent".to_string(),
            "bitcoin-lightning-bolt11".to_string(),
            10,
            None,
        )
        .expect("submit invoice");

    let required = client_recv(&mut client_rx, 2);
    assert!(
        required.contains("payment_required"),
        "client sees payment_required: {required}"
    );
    assert!(
        required.contains("lnbc1transparent"),
        "payment_required carries pay_req: {required}"
    );

    // Mark settled. Client receives payment_accepted BEFORE the handler result.
    server
        .mark_payment_settled("lnbc1transparent".to_string(), None)
        .expect("mark settled");

    let accepted = client_recv(&mut client_rx, 2);
    assert!(
        accepted.contains("payment_accepted"),
        "client sees payment_accepted: {accepted}"
    );

    // The forwarded request now appears on the server consumer channel.
    let incoming = server
        .recv_timeout(2)
        .expect("server receives paid request");
    server
        .send_response(
            &incoming.event_id,
            &response(serde_json::json!(incoming.message.id)),
        )
        .expect("send response");

    let result = client_recv(&mut client_rx, 2);
    assert!(
        result.contains(r#""tools":[]"#),
        "client receives handler result after payment_accepted: {result}"
    );

    server.close().expect("close server");
    let _ = global_runtime().block_on(client.close());
}

#[test]
fn gating_park_settle_and_retry() {
    let (server_keys, server_sdk_keys, client_pool, server_pool) = fresh_fixture();

    let server_config = ServerConfig {
        encryption_mode: EncryptionMode::Disabled,
        ..Default::default()
    };
    let server = make_server_with_pool(&server_keys, &server_config, server_pool);

    server
        .set_payment_interaction_policy(PaymentInteractionPolicy::Optional)
        .expect("set optional policy");
    server
        .set_priced_capabilities_json(
            r#"[{"method":"tools/call","name":"download_media","amount":1000,"currencyUnit":"sats"}]"#,
        )
        .expect("set priced capabilities");
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
            .expect("create client");
        client.start().await.expect("start client");
        let client_rx = client.take_message_receiver().expect("client receiver");
        tokio::time::sleep(Duration::from_millis(50)).await;
        client
            .send(&tools_call("g1", "download_media"))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        (client, client_rx)
    });

    let event = server
        .recv_payment_gate_request(2)
        .expect("receive payment gate request")
        .expect("payment gate request present");

    // In gating, submit_invoice answers the original request with a targeted -32042.
    server
        .submit_invoice(
            event.request_event_id.clone(),
            1000,
            "lnbc1gating".to_string(),
            "bitcoin-lightning-bolt11".to_string(),
            10,
            None,
        )
        .expect("submit invoice");

    let required = client_recv(&mut client_rx, 2);
    assert!(
        required.contains("-32042"),
        "client sees payment_required error: {required}"
    );
    assert!(
        required.contains("lnbc1gating"),
        "error carries pay_req: {required}"
    );

    // Retry while pending should receive -32043 payment_pending.
    global_runtime().block_on(async {
        client
            .send(&tools_call("g1-retry-1", "download_media"))
            .await
            .expect("send retry");
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    let pending = client_recv(&mut client_rx, 2);
    assert!(
        pending.contains("-32043"),
        "client sees payment_pending: {pending}"
    );

    // Settle. The client must retry again to consume the grant.
    server
        .mark_payment_settled("lnbc1gating".to_string(), None)
        .expect("mark settled");

    // Wait for settle to persist before the next retry.
    global_runtime().block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });

    global_runtime().block_on(async {
        client
            .send(&tools_call("g1-retry-2", "download_media"))
            .await
            .expect("send retry");
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    let incoming = server
        .recv_timeout(2)
        .expect("server receives retried request");
    server
        .send_response(
            &incoming.event_id,
            &response(serde_json::json!(incoming.message.id)),
        )
        .expect("send response");

    let result = client_recv(&mut client_rx, 2);
    assert!(
        result.contains(r#""tools":[]"#),
        "client receives handler result after gating retry: {result}"
    );

    server.close().expect("close server");
    let _ = global_runtime().block_on(client.close());
}

#[test]
fn gating_restart_rebinds_same_payreq() {
    let (server_keys, server_sdk_keys, client_pool, server_pool) = fresh_fixture();

    let server_config = ServerConfig {
        encryption_mode: EncryptionMode::Disabled,
        ..Default::default()
    };

    // First server session.
    let server = make_server_with_pool(&server_keys, &server_config, server_pool.clone());
    server
        .set_payment_interaction_policy(PaymentInteractionPolicy::Optional)
        .expect("set optional policy");
    server
        .set_priced_capabilities_json(
            r#"[{"method":"tools/call","name":"download_media","amount":1000,"currencyUnit":"sats"}]"#,
        )
        .expect("set priced capabilities");
    server.start().expect("start first server");

    let client_config = sdk::NostrClientTransportConfig::default()
        .with_relay_urls(vec!["wss://mock.relay".to_string()])
        .with_server_pubkey(server_sdk_keys.public_key().to_hex())
        .with_encryption_mode(sdk::EncryptionMode::Disabled)
        .with_pmis(vec!["bitcoin-lightning-bolt11".to_string()])
        .with_payment_interaction(sdk::PaymentInteractionMode::ExplicitGating);

    let (mut client, mut client_rx) = global_runtime().block_on(async {
        let mut client = sdk::NostrClientTransport::with_relay_pool(client_config, client_pool)
            .await
            .expect("create client");
        client.start().await.expect("start client");
        let client_rx = client.take_message_receiver().expect("client receiver");
        tokio::time::sleep(Duration::from_millis(50)).await;
        client
            .send(&tools_call("r1", "download_media"))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        (client, client_rx)
    });

    let event = server
        .recv_payment_gate_request(2)
        .expect("receive payment gate request")
        .expect("payment gate request present");
    server
        .submit_invoice(
            event.request_event_id.clone(),
            1000,
            "lnbc1restart".to_string(),
            "bitcoin-lightning-bolt11".to_string(),
            10,
            None,
        )
        .expect("submit invoice");

    let required = client_recv(&mut client_rx, 2);
    assert!(
        required.contains("-32042"),
        "client sees payment_required on first session"
    );

    server.close().expect("close first server");
    let _ = global_runtime().block_on(client.close());

    // Restart with a fresh mock network (process restart). The client reconnects.
    let (client_pool2, server_pool2) = sdk::relay::MockRelayPool::create_pair();
    let client_pool2 = Arc::new(client_pool2);
    let server_pool2 = Arc::new(server_pool2.linked_with_keys(server_sdk_keys.clone()));

    let server2 = make_server_with_pool(&server_keys, &server_config, server_pool2);
    server2
        .set_payment_interaction_policy(PaymentInteractionPolicy::Optional)
        .expect("set optional policy");
    server2
        .set_priced_capabilities_json(
            r#"[{"method":"tools/call","name":"download_media","amount":1000,"currencyUnit":"sats"}]"#,
        )
        .expect("set priced capabilities");
    server2.start().expect("start second server");

    let (mut client, mut client_rx) = global_runtime().block_on(async {
        let client_config = sdk::NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_sdk_keys.public_key().to_hex())
            .with_encryption_mode(sdk::EncryptionMode::Disabled)
            .with_pmis(vec!["bitcoin-lightning-bolt11".to_string()])
            .with_payment_interaction(sdk::PaymentInteractionMode::ExplicitGating);
        let mut client = sdk::NostrClientTransport::with_relay_pool(client_config, client_pool2)
            .await
            .expect("create client after restart");
        client.start().await.expect("start client after restart");
        let client_rx = client.take_message_receiver().expect("client receiver");
        tokio::time::sleep(Duration::from_millis(50)).await;
        client
            .send(&tools_call("r2", "download_media"))
            .await
            .expect("send retry after restart");
        tokio::time::sleep(Duration::from_millis(50)).await;
        (client, client_rx)
    });

    let rebind = server2
        .recv_payment_gate_request(2)
        .expect("receive rebind payment gate request")
        .expect("payment gate request present");
    server2
        .submit_invoice(
            rebind.request_event_id,
            1000,
            "lnbc1restart".to_string(),
            "bitcoin-lightning-bolt11".to_string(),
            10,
            None,
        )
        .expect("rebind same pay_req");

    // Gating: the rebind request itself is answered with a fresh -32042 before
    // settlement; the next retry after mark_settled claims the grant.
    let rebind_required = client_recv(&mut client_rx, 2);
    assert!(
        rebind_required.contains("-32042"),
        "client sees rebind payment_required: {rebind_required}"
    );

    server2
        .mark_payment_settled("lnbc1restart".to_string(), None)
        .expect("mark settled after rebind");

    global_runtime().block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });

    global_runtime().block_on(async {
        client
            .send(&tools_call("r3", "download_media"))
            .await
            .expect("send final retry");
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    let incoming = server2
        .recv_timeout(2)
        .expect("server receives final retry");
    server2
        .send_response(
            &incoming.event_id,
            &response(serde_json::json!(incoming.message.id)),
        )
        .expect("send response");

    let result = client_recv(&mut client_rx, 2);
    assert!(
        result.contains(r#""tools":[]"#),
        "client receives handler result after restart rebind: {result}"
    );

    server2.close().expect("close second server");
    let _ = global_runtime().block_on(client.close());
}

#[test]
fn route_budget_response_deliverable() {
    let (server_keys, server_sdk_keys, client_pool, server_pool) = fresh_fixture();

    // Use explicit short timeouts that still satisfy the payment-route budget.
    let server_config = ServerConfig {
        encryption_mode: EncryptionMode::Disabled,
        payment_ttl_cap_secs: 2,
        execution_budget_secs: 1,
        request_timeout_secs: Some(70),
        session_timeout_secs: Some(120),
        ..Default::default()
    };
    let server = make_server_with_pool(&server_keys, &server_config, server_pool);

    server
        .set_payment_interaction_policy(PaymentInteractionPolicy::Transparent)
        .expect("set transparent policy");
    server
        .set_priced_capabilities_json(
            r#"[{"method":"tools/call","name":"download_media","amount":1000,"currencyUnit":"sats"}]"#,
        )
        .expect("set priced capabilities");
    server.start().expect("start server");

    let client_config = sdk::NostrClientTransportConfig::default()
        .with_relay_urls(vec!["wss://mock.relay".to_string()])
        .with_server_pubkey(server_sdk_keys.public_key().to_hex())
        .with_encryption_mode(sdk::EncryptionMode::Disabled)
        .with_pmis(vec!["bitcoin-lightning-bolt11".to_string()]);

    let (mut client, mut client_rx) = global_runtime().block_on(async {
        let mut client = sdk::NostrClientTransport::with_relay_pool(client_config, client_pool)
            .await
            .expect("create client");
        client.start().await.expect("start client");
        let client_rx = client.take_message_receiver().expect("client receiver");
        tokio::time::sleep(Duration::from_millis(50)).await;
        client
            .send(&tools_call("rb1", "download_media"))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        (client, client_rx)
    });

    let event = server
        .recv_payment_gate_request(2)
        .expect("receive payment gate request")
        .expect("payment gate request present");
    server
        .submit_invoice(
            event.request_event_id,
            1000,
            "lnbc1route".to_string(),
            "bitcoin-lightning-bolt11".to_string(),
            2,
            None,
        )
        .expect("submit invoice");

    let required = client_recv(&mut client_rx, 2);
    assert!(
        required.contains("payment_required"),
        "client sees payment_required"
    );

    server
        .mark_payment_settled("lnbc1route".to_string(), None)
        .expect("mark settled");

    let accepted = client_recv(&mut client_rx, 2);
    assert!(
        accepted.contains("payment_accepted"),
        "client sees payment_accepted before result: {accepted}"
    );

    let incoming = server
        .recv_timeout(2)
        .expect("server receives paid request");
    server
        .send_response(
            &incoming.event_id,
            &response(serde_json::json!(incoming.message.id)),
        )
        .expect("send response");

    let result = client_recv(&mut client_rx, 2);
    assert!(
        result.contains(r#""tools":[]"#),
        "client receives result within route budget: {result}"
    );

    server.close().expect("close server");
    let _ = global_runtime().block_on(client.close());
}

#[test]
fn mark_replayed_no_invoice() {
    let (server_keys, server_sdk_keys, client_pool, server_pool) = fresh_fixture();
    let client_pool_for_find = client_pool.clone();

    let server_config = ServerConfig {
        encryption_mode: EncryptionMode::Disabled,
        ..Default::default()
    };
    let server = make_server_with_pool(&server_keys, &server_config, server_pool);

    server
        .set_payment_interaction_policy(PaymentInteractionPolicy::Transparent)
        .expect("set transparent policy");
    server
        .set_priced_capabilities_json(
            r#"[{"method":"tools/call","name":"download_media","amount":1000,"currencyUnit":"sats"}]"#,
        )
        .expect("set priced capabilities");
    server.start().expect("start server");

    let client_config = sdk::NostrClientTransportConfig::default()
        .with_relay_urls(vec!["wss://mock.relay".to_string()])
        .with_server_pubkey(server_sdk_keys.public_key().to_hex())
        .with_encryption_mode(sdk::EncryptionMode::Disabled)
        .with_pmis(vec!["bitcoin-lightning-bolt11".to_string()]);

    let (mut client, mut client_rx) = global_runtime().block_on(async {
        let mut client = sdk::NostrClientTransport::with_relay_pool(client_config, client_pool)
            .await
            .expect("create client");
        client.start().await.expect("start client");
        let client_rx = client.take_message_receiver().expect("client receiver");
        tokio::time::sleep(Duration::from_millis(50)).await;
        client
            .send(&tools_call("mr1", "download_media"))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        (client, client_rx)
    });

    let event = server
        .recv_payment_gate_request(2)
        .expect("receive payment gate request")
        .expect("payment gate request present");

    // Mark the parked request as replayed without ever submitting an invoice.
    server
        .mark_replayed(event.request_event_id.clone())
        .expect("mark replayed");

    // No payment notification should have been emitted.
    assert!(
        find_notification(
            &client_pool_for_find,
            &server_sdk_keys.public_key().to_hex(),
            "payment_required"
        )
        .is_none(),
        "mark_replayed must not emit payment_required"
    );

    let incoming = server
        .recv_timeout(2)
        .expect("server receives replayed request");
    server
        .send_response(
            &incoming.event_id,
            &response(serde_json::json!(incoming.message.id)),
        )
        .expect("send response");

    let result = client_recv(&mut client_rx, 2);
    assert!(
        result.contains(r#""tools":[]"#),
        "client receives result for replayed request: {result}"
    );

    server.close().expect("close server");
    let _ = global_runtime().block_on(client.close());
}

fn client_recv(
    client_rx: &mut tokio::sync::mpsc::UnboundedReceiver<sdk::JsonRpcMessage>,
    timeout_secs: u64,
) -> String {
    let msg = global_runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(timeout_secs), client_rx.recv())
            .await
            .expect("client receives a message")
            .expect("client channel is open")
    });
    serde_json::to_string(&msg).unwrap()
}

fn find_notification(
    client_pool: &Arc<sdk::relay::MockRelayPool>,
    server_pubkey: &str,
    method: &str,
) -> Option<serde_json::Value> {
    let events = global_runtime().block_on(client_pool.stored_events());
    for event in events {
        if event.pubkey.to_hex() != server_pubkey {
            continue;
        }
        if let Ok(sdk::JsonRpcMessage::Notification(n)) =
            serde_json::from_str::<sdk::JsonRpcMessage>(&event.content)
        {
            if n.method == method {
                return Some(n.params.unwrap_or(serde_json::Value::Null));
            }
        }
    }
    None
}
