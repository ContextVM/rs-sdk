//! CEP-8 bilateral payments, end to end over `MockRelayPool`: a client built
//! with `with_client_payments` against a server built with
//! `with_server_payments`, both lifecycles driven through the production
//! registration entry points, and DELIVERY asserted at the client's own
//! channel (not only on the wire).
//!
//! Clock discipline: the correlation-retention sweep, heartbeat stops, and
//! session expiry all age on `std::time::Instant`, which paused tokio time
//! does not advance, so every test here runs on the real clock with tiny
//! configured timeouts. A test that mixes the paused tokio clock with an
//! Instant-aged assertion proves nothing while green, which is why none does.
//! Retries are never scheduled under one second apart: a byte-identical retry
//! in the same wall-clock second mints the same Nostr event id (created_at
//! has second resolution), which relays and the server's ingestion dedup then
//! swallow.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contextvm_sdk::core::constants::CTXVM_MESSAGES_KIND;
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::payments::fakes::{FakePaymentProcessor, FakePaymentProcessorOptions};
use contextvm_sdk::payments::types::PricedCapability;
use contextvm_sdk::payments::{
    with_client_payments, with_server_payments, ClientPaymentsOptions, OnPaymentRequiredFn,
    PaymentApproval, PaymentError, PaymentHandler, PaymentHandlerRequest, PaymentPolicyFn,
    ServerPaymentsOptions,
};
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::base::BaseTransport;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{
    IncomingRequest, NostrServerTransport, NostrServerTransportConfig,
};
use contextvm_sdk::{
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, PaymentInteractionMode, RelayPoolTrait,
};
use futures::FutureExt;
use nostr_sdk::prelude::*;

fn as_pool(pool: &Arc<MockRelayPool>) -> Arc<dyn RelayPoolTrait> {
    Arc::clone(pool) as Arc<dyn RelayPoolTrait>
}

fn paid_call(id: &str) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({ "name": "paid-tool" })),
    })
}

/// A priced call carrying an rmcp-shaped NUMERIC progress token.
fn paid_streaming_call(id: &str, token: i64) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "paid-tool",
            "_meta": { "progressToken": token },
        })),
    })
}

fn free_call(id: &str) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({ "name": "free-tool" })),
    })
}

fn result_response(id: &str) -> JsonRpcMessage {
    JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        result: serde_json::json!({ "content": [] }),
    })
}

fn priced_tool(amount: i64) -> PricedCapability {
    PricedCapability {
        method: "tools/call".to_string(),
        name: Some("paid-tool".to_string()),
        amount,
        max_amount: None,
        currency_unit: "sats".to_string(),
        description: None,
    }
}

fn fake_processor(pmi: &str, verify_delay_ms: u64) -> Arc<FakePaymentProcessor> {
    Arc::new(FakePaymentProcessor::with_options(
        FakePaymentProcessorOptions {
            pmi: pmi.to_string(),
            verify_delay_ms,
            create_delay_ms: 0,
            ttl: None,
        },
    ))
}

/// The standard one-processor, one-priced-tool server configuration.
fn server_options(verify_delay_ms: u64) -> ServerPaymentsOptions {
    ServerPaymentsOptions::new(
        vec![fake_processor("fake", verify_delay_ms)],
        vec![priced_tool(21)],
    )
}

/// A wallet handler that counts its settled payments.
struct CountingHandler {
    pmi: String,
    delay: Duration,
    calls: Arc<AtomicUsize>,
}

impl CountingHandler {
    fn new(calls: Arc<AtomicUsize>) -> Self {
        Self {
            pmi: "fake".to_string(),
            delay: Duration::from_millis(10),
            calls,
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[async_trait]
impl PaymentHandler for CountingHandler {
    fn pmi(&self) -> &str {
        &self.pmi
    }
    async fn handle(&self, _req: PaymentHandlerRequest) -> Result<(), PaymentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        Ok(())
    }
}

/// A gating callback that "pays" (the fake processor settles by itself) after
/// `delay_ms` and approves the retry. The delay keeps every retry at least a
/// second away from the request it retries (the event-id fixture rule above),
/// and counts invocations.
fn paying_callback(delay_ms: u64, calls: Arc<AtomicUsize>) -> Arc<OnPaymentRequiredFn> {
    Arc::new(move |_params| {
        let calls = Arc::clone(&calls);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            Ok(PaymentApproval {
                paid: true,
                reason: None,
            })
        }
        .boxed()
    })
}

fn declining_callback(reason: &str) -> Arc<OnPaymentRequiredFn> {
    let reason = reason.to_string();
    Arc::new(move |_params| {
        let reason = reason.clone();
        async move {
            Ok(PaymentApproval {
                paid: false,
                reason: Some(reason),
            })
        }
        .boxed()
    })
}

fn all_tags(event: &Event) -> Vec<Vec<String>> {
    event.tags.iter().map(|t| t.clone().to_vec()).collect()
}

/// Every stored server-authored ContextVM event whose content contains `needle`.
async fn server_events_containing(
    pool: &Arc<MockRelayPool>,
    server: PublicKey,
    needle: &str,
) -> Vec<Event> {
    pool.stored_events()
        .await
        .into_iter()
        .filter(|e| {
            e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)
                && e.pubkey == server
                && e.content.contains(needle)
        })
        .collect()
}

/// Every stored client request event whose content contains the quoted `id`.
async fn client_request_events(
    pool: &Arc<MockRelayPool>,
    client: PublicKey,
    id: &str,
) -> Vec<Event> {
    let needle = format!("\"{id}\"");
    pool.stored_events()
        .await
        .into_iter()
        .filter(|e| {
            e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)
                && e.pubkey == client
                && e.content.contains(&needle)
        })
        .collect()
}

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

struct Fx {
    server: NostrServerTransport,
    server_rx: tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
    client: NostrClientTransport,
    client_rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    pool: Arc<MockRelayPool>,
    client_pool: Arc<MockRelayPool>,
    client_pubkey: PublicKey,
    server_pubkey: PublicKey,
}

/// A paired client/server, BOTH configured through the production payment
/// registration entry points before `start()`. Fixture rule, load-bearing:
/// each test's priced call is its session's first message, so the one-shot
/// discovery and negotiation surfaces ride it.
async fn fixture(
    srv_options: ServerPaymentsOptions,
    client_options: ClientPaymentsOptions,
    configure_server: impl FnOnce(NostrServerTransportConfig) -> NostrServerTransportConfig,
    configure_client: impl FnOnce(NostrClientTransportConfig) -> NostrClientTransportConfig,
) -> Fx {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let pool = Arc::new(server_pool);
    let client_pool = Arc::new(client_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        configure_server(
            NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        ),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    with_server_payments(&mut server, srv_options).expect("register server payments");

    let mut client = NostrClientTransport::with_relay_pool(
        configure_client(
            NostrClientTransportConfig::default()
                .with_relay_urls(vec!["wss://mock.relay".to_string()])
                .with_server_pubkey(server_pubkey.to_hex())
                .with_encryption_mode(EncryptionMode::Disabled)
                .with_timeout(Duration::from_secs(30)),
        ),
        as_pool(&client_pool),
    )
    .await
    .expect("client transport");
    with_client_payments(&mut client, client_options).expect("register client payments");

    let server_rx = server.take_message_receiver().expect("server rx");
    let client_rx = client.take_message_receiver().expect("client rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    Fx {
        server,
        server_rx,
        client,
        client_rx,
        pool,
        client_pool,
        client_pubkey,
        server_pubkey,
    }
}

// ── the transparent capstone ────────────────────────────────────────────────

/// The capstone: a priced call from an auto-paying client delivers end to end,
/// and the consumer observes the EXACT sequence: the immediate synthetic beat
/// first, then the invoice notification, the acceptance, and the real result.
/// The client's `pmi` advertisement on the wire equals the handler PMIs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transparent_auto_pay_delivers_end_to_end() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(300),
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))]),
        |c| c,
        |c| c,
    )
    .await;

    fx.client
        .send(&paid_streaming_call("pay-b1", 7))
        .await
        .expect("send");

    // The paid request reaches the handler after settlement; answer it.
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the tool handler")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("pay-b1"))
        .await
        .expect("respond");

    // The consumer-observable sequence, EXACT: beat, invoice, acceptance,
    // result. The beat precedes the forwarded invoice (the immediate beat is
    // pushed synchronously before the notification), and nothing is delivered
    // twice.
    let beat = recv_within(&mut fx.client_rx, 3000, "the immediate beat").await;
    match beat {
        JsonRpcMessage::Notification(n) => {
            assert_eq!(n.method, "notifications/progress");
            assert_eq!(
                n.params,
                Some(serde_json::json!({ "progressToken": 7, "progress": 0 })),
                "the beat carries the original numeric token"
            );
        }
        other => panic!("expected the immediate beat first, got {other:?}"),
    }
    let invoice = recv_within(&mut fx.client_rx, 3000, "the invoice").await;
    match invoice {
        JsonRpcMessage::Notification(n) => {
            assert_eq!(n.method, "notifications/payment_required");
        }
        other => panic!("expected the invoice second, got {other:?}"),
    }
    let accepted = recv_within(&mut fx.client_rx, 3000, "the acceptance").await;
    match accepted {
        JsonRpcMessage::Notification(n) => {
            assert_eq!(n.method, "notifications/payment_accepted");
        }
        other => panic!("expected the acceptance third, got {other:?}"),
    }
    let result = recv_within(&mut fx.client_rx, 3000, "the result").await;
    match result {
        JsonRpcMessage::Response(r) => assert_eq!(r.id, serde_json::json!("pay-b1")),
        other => panic!("expected the result last, got {other:?}"),
    }
    assert!(
        fx.client_rx.try_recv().is_err(),
        "nothing may be delivered twice"
    );

    // One settled payment.
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the handler paid once");

    // The wire advertisement is the HANDLER'S PMI list (it replaced the
    // config's), riding the priced call.
    let request = client_request_events(&fx.pool, fx.client_pubkey, "pay-b1").await[0].clone();
    let pmi_tags: Vec<Vec<String>> = all_tags(&request)
        .into_iter()
        .filter(|t| t.first().map(String::as_str) == Some("pmi"))
        .collect();
    assert_eq!(
        pmi_tags,
        vec![vec!["pmi".to_string(), "fake".to_string()]],
        "the advertised PMIs must be exactly the handler PMIs"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// A payment that outlives the client's correlation-retention sweep still
/// delivers: the per-payment touch loop refreshes the pending entry on its
/// bounded cadence while the wallet settles, so the result arrives at the
/// client channel minutes (here: several sweep windows) after the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn touch_loop_keeps_a_slow_payment_alive_past_the_sweep() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(2500),
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))])
            .with_synthetic_progress_interval(Duration::from_millis(100)),
        |c| c,
        // A tiny retention TTL: the sweep samples every second (its clamp
        // floor) and would evict an untouched entry on its first tick, long
        // before the 2.5 s settlement.
        |c| c.with_timeout(Duration::from_millis(300)),
    )
    .await;

    fx.client
        .send(&paid_streaming_call("pay-b2", 9))
        .await
        .expect("send");

    let incoming = tokio::time::timeout(Duration::from_secs(5), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the tool handler")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("pay-b2"))
        .await
        .expect("respond");

    // Drain the channel until the result: beats and the two payment
    // notifications precede it; the observable that matters is DELIVERY.
    let mut beats = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let delivered = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the paid result must be delivered despite the sweep"
        );
        match recv_within(&mut fx.client_rx, 5000, "lifecycle traffic").await {
            JsonRpcMessage::Response(r) => break r,
            JsonRpcMessage::Notification(n) => {
                if n.method == "notifications/progress" {
                    beats += 1;
                }
            }
            other => panic!("unexpected client-channel message {other:?}"),
        }
    };
    assert_eq!(delivered.id, serde_json::json!("pay-b2"));
    assert!(
        beats >= 2,
        "the heartbeat must have beaten across the payment window, saw {beats}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

// ── explicit gating ─────────────────────────────────────────────────────────

/// Explicit gating end to end: the offer is intercepted (never surfaced), the
/// callback pays, the engine retries the SAME request under a fresh event, the
/// grant claims, and the result is delivered. The consumer never sees a raw
/// gating error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gating_pay_and_retry_delivers_end_to_end() {
    let paid = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(50),
        ClientPaymentsOptions::new()
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::new(
                AtomicUsize::new(0),
            )))])
            .with_on_payment_required(paying_callback(1100, Arc::clone(&paid))),
        |c| c,
        |c| c,
    )
    .await;

    fx.client.send(&paid_call("pay-g1")).await.expect("send");

    // The tool runs exactly once, after the paid retry claims the grant.
    let incoming = tokio::time::timeout(Duration::from_secs(6), fx.server_rx.recv())
        .await
        .expect("the paid retry must reach the tool handler")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("pay-g1"))
        .await
        .expect("respond");

    // The consumer sees ONLY the result: no raw Payment Required, no raw
    // Payment Pending, nothing synthesized on the happy path.
    let result = recv_within(&mut fx.client_rx, 3000, "the result").await;
    match result {
        JsonRpcMessage::Response(r) => assert_eq!(r.id, serde_json::json!("pay-g1")),
        other => panic!("the consumer must see only the result, got {other:?}"),
    }
    assert!(fx.client_rx.try_recv().is_err());
    assert_eq!(paid.load(Ordering::SeqCst), 1, "one payment round");

    // The wire shows the offer plus at least one retry: fresh outer events,
    // identical inner content (id, method, params).
    let requests = client_request_events(&fx.pool, fx.client_pubkey, "pay-g1").await;
    assert!(
        requests.len() >= 2,
        "the original and at least one retry, got {}",
        requests.len()
    );
    let inner: Vec<serde_json::Value> = requests
        .iter()
        .map(|e| serde_json::from_str(&e.content).expect("request content"))
        .collect();
    for later in &inner[1..] {
        assert_eq!(
            later, &inner[0],
            "every retry must replay the original request byte-true"
        );
    }
    let mut ids: Vec<EventId> = requests.iter().map(|e| e.id).collect();
    ids.dedup();
    assert_eq!(ids.len(), requests.len(), "each attempt is a fresh event");
    assert_eq!(
        server_events_containing(&fx.pool, fx.server_pubkey, "\"code\":-32042")
            .await
            .len(),
        1,
        "exactly one offer was issued"
    );
    assert!(
        fx.server_rx.try_recv().is_err(),
        "the tool ran exactly once"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// A retry that lands while verification is still running draws `-32043`; the
/// engine backs off (never under a second) and retries until the grant claims,
/// and the consumer sees neither code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_backoff_resolves_and_delivers() {
    let paid = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(1500),
        ClientPaymentsOptions::new()
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_on_payment_required(paying_callback(1100, Arc::clone(&paid))),
        |c| c,
        |c| c,
    )
    .await;

    fx.client.send(&paid_call("pay-g2")).await.expect("send");

    let incoming = tokio::time::timeout(Duration::from_secs(10), fx.server_rx.recv())
        .await
        .expect("the retried request must eventually claim the grant")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("pay-g2"))
        .await
        .expect("respond");

    let result = recv_within(&mut fx.client_rx, 3000, "the result").await;
    match result {
        JsonRpcMessage::Response(r) => assert_eq!(r.id, serde_json::json!("pay-g2")),
        other => panic!("the consumer must see only the result, got {other:?}"),
    }
    assert!(
        fx.client_rx.try_recv().is_err(),
        "neither gating code may reach the consumer"
    );

    // The server answered at least one pending round on the wire, and the
    // client re-sent the request for it (three or more attempts in total).
    assert!(
        !server_events_containing(&fx.pool, fx.server_pubkey, "\"code\":-32043")
            .await
            .is_empty(),
        "at least one pending round was played"
    );
    assert!(
        client_request_events(&fx.pool, fx.client_pubkey, "pay-g2")
            .await
            .len()
            >= 3,
        "the original, the paid retry, and at least one backoff retry"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// A user decline surfaces the reason and never retries: the consumer receives
/// a synthesized Payment Required error carrying `data.reason` on the original
/// request id, no retry rides the wire, and the tool never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_decline_surfaces_reason_and_never_retries() {
    let mut fx = fixture(
        server_options(50),
        ClientPaymentsOptions::new()
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_on_payment_required(declining_callback("user said no")),
        |c| c,
        |c| c,
    )
    .await;

    fx.client.send(&paid_call("pay-d1")).await.expect("send");

    let surfaced = recv_within(&mut fx.client_rx, 3000, "the synthesized decline").await;
    match surfaced {
        JsonRpcMessage::ErrorResponse(e) => {
            assert_eq!(e.id, serde_json::json!("pay-d1"));
            assert_eq!(e.error.code, -32042);
            assert_eq!(e.error.message, "Payment Required");
            assert_eq!(
                e.error.data,
                Some(serde_json::json!({ "reason": "user said no" }))
            );
        }
        other => panic!("expected the synthesized decline, got {other:?}"),
    }

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        client_request_events(&fx.pool, fx.client_pubkey, "pay-d1")
            .await
            .len(),
        1,
        "a decline must never retry"
    );
    assert!(fx.server_rx.try_recv().is_err(), "the tool never ran");

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// When the retry budget is spent, the RAW Payment Pending error surfaces on
/// the original id, exactly as the server sent it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_exhausted_surfaces_the_raw_pending_error() {
    let paid = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(10_000),
        ClientPaymentsOptions::new()
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_on_payment_required(paying_callback(1100, Arc::clone(&paid)))
            .with_max_pending_retries(1),
        |c| c,
        |c| c,
    )
    .await;

    fx.client.send(&paid_call("pay-x1")).await.expect("send");

    // Paid retry, one pending retry, then the raw give-up.
    let surfaced = recv_within(&mut fx.client_rx, 8000, "the raw pending error").await;
    match surfaced {
        JsonRpcMessage::ErrorResponse(e) => {
            assert_eq!(e.id, serde_json::json!("pay-x1"));
            assert_eq!(e.error.code, -32043);
            assert_eq!(e.error.message, "Payment Pending");
        }
        other => panic!("expected the raw pending error, got {other:?}"),
    }
    assert_eq!(
        client_request_events(&fx.pool, fx.client_pubkey, "pay-x1")
            .await
            .len(),
        3,
        "the original, the paid retry, and exactly one pending retry"
    );
    assert!(fx.server_rx.try_recv().is_err(), "the tool never ran");

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

// ── replay defense and double-pay ───────────────────────────────────────────

/// Neither a stranger-signed offer correlated to a live request nor the
/// server's own replayed offer for a completed one ever reaches the wallet:
/// the sender-authentication gate drops the first and the correlation gate
/// drops the second, both before the engine runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_and_replayed_offers_never_reach_the_handler() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(100),
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))]),
        |c| c,
        |c| c,
    )
    .await;

    // A live pending request the server will not answer yet (unpriced, and we
    // withhold the response).
    fx.client.send(&free_call("free-1")).await.expect("send");
    let free_incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the free call reaches the handler")
        .expect("server channel open");

    // Face one: a STRANGER signs a perfectly-shaped offer correlated to the
    // live request and addressed to the client. The sender-authentication
    // gate drops it before parse; the wallet stays silent.
    let stranger = Arc::new(fx.pool.linked_with_keys(Keys::generate()));
    let stranger_base = BaseTransport {
        relay_pool: Arc::clone(&stranger) as Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode::Disabled,
        is_connected: true,
    };
    let forged = JsonRpcMessage::Notification(contextvm_sdk::JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/payment_required".to_string(),
        params: Some(serde_json::json!({
            "amount": 1_000_000,
            "pay_req": "forged-invoice",
            "pmi": "fake",
        })),
    });
    let request_event_id = EventId::from_hex(&free_incoming.event_id).expect("event id");
    let tags = BaseTransport::create_response_tags(&fx.client_pubkey, &request_event_id);
    stranger_base
        .send_mcp_message(
            &forged,
            &fx.client_pubkey,
            CTXVM_MESSAGES_KIND,
            tags,
            Some(false),
            None,
        )
        .await
        .expect("publish the forged offer");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a forged offer must never reach the wallet"
    );
    assert!(
        fx.client_rx.try_recv().is_err(),
        "the forged offer is dropped before the consumer too"
    );

    // A real paid flow completes (one wallet call), so a replayable offer and
    // a consumed correlation entry exist.
    fx.client.send(&paid_call("pay-r1")).await.expect("send");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request reaches the handler")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("pay-r1"))
        .await
        .expect("respond");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the paid result must deliver"
        );
        if let JsonRpcMessage::Response(_) =
            recv_within(&mut fx.client_rx, 3000, "lifecycle traffic").await
        {
            break;
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one real payment settled");

    // Face two: the server's own signed offer REPLAYED after completion. The
    // correlation gate drops it (no live pending entry); the wallet count
    // stays at one.
    let invoice = server_events_containing(&fx.pool, fx.server_pubkey, "payment_required")
        .await
        .into_iter()
        .find(|e| all_tags(e).contains(&vec!["e".to_string(), incoming.event_id.clone()]))
        .expect("the stored offer");
    fx.pool.publish_event(&invoice).await.expect("replay");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a replayed offer must never reach the wallet again"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// A duplicate delivery of one offer while the wallet is mid-flight settles
/// exactly one payment: the in-flight claim is taken synchronously before the
/// handler chain is spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_offer_is_paid_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(800),
        ClientPaymentsOptions::new().with_handlers(vec![Arc::new(
            CountingHandler::new(Arc::clone(&calls)).with_delay(Duration::from_millis(500)),
        )]),
        |c| c,
        |c| c,
    )
    .await;

    fx.client.send(&paid_call("pay-dup")).await.expect("send");

    // Wait for the offer to reach the consumer (the engine has claimed it and
    // the wallet is now sleeping mid-payment), then redeliver the same event.
    let invoice_msg = recv_within(&mut fx.client_rx, 3000, "the invoice").await;
    assert!(matches!(invoice_msg, JsonRpcMessage::Notification(_)));
    let invoice = server_events_containing(&fx.pool, fx.server_pubkey, "payment_required")
        .await
        .into_iter()
        .next()
        .expect("the stored offer");
    fx.pool.publish_event(&invoice).await.expect("redeliver");

    // Settlement, forwarding, and the response all complete normally.
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request reaches the handler")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("pay-dup"))
        .await
        .expect("respond");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        assert!(tokio::time::Instant::now() < deadline, "result delivers");
        if let JsonRpcMessage::Response(_) =
            recv_within(&mut fx.client_rx, 3000, "lifecycle traffic").await
        {
            break;
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one wallet action per in-flight payment request"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

// ── mode upsert, reconnect, oversized, out-of-band ──────────────────────────

/// A mid-session `payment_interaction` upsert flips the lifecycle: the same
/// client pays one call transparently, requests explicit gating, and the next
/// identical call runs the gated flow (no authorization migrates from the
/// completed transparent payment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_session_upsert_flips_the_lifecycle_bilaterally() {
    let calls = Arc::new(AtomicUsize::new(0));
    let paid = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(50),
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))])
            .with_on_payment_required(paying_callback(1100, Arc::clone(&paid))),
        |c| c,
        |c| c,
    )
    .await;

    // Call one: the default (transparent) lifecycle pays and delivers.
    fx.client.send(&paid_call("ups-1")).await.expect("send");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the first call settles transparently")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("ups-1"))
        .await
        .expect("respond");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "result one delivers"
        );
        if let JsonRpcMessage::Response(r) =
            recv_within(&mut fx.client_rx, 3000, "lifecycle traffic").await
        {
            assert_eq!(r.id, serde_json::json!("ups-1"));
            break;
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "call one paid in-band");

    // The upsert: this session now requires explicit gating.
    fx.client
        .set_payment_interaction(PaymentInteractionMode::ExplicitGating);

    // Call two, identical params: gated, paid through the callback, and
    // delivered. The completed transparent payment migrated nothing, so a
    // fresh offer MUST appear on the wire.
    fx.client.send(&paid_call("ups-2")).await.expect("send");
    let incoming = tokio::time::timeout(Duration::from_secs(6), fx.server_rx.recv())
        .await
        .expect("the gated retry claims and forwards")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("ups-2"))
        .await
        .expect("respond");
    let result = recv_within(&mut fx.client_rx, 3000, "the second result").await;
    match result {
        JsonRpcMessage::Response(r) => assert_eq!(r.id, serde_json::json!("ups-2")),
        other => panic!("expected the gated result, got {other:?}"),
    }
    assert_eq!(
        server_events_containing(&fx.pool, fx.server_pubkey, "\"code\":-32042")
            .await
            .len(),
        1,
        "the gated call must draw a fresh offer: nothing migrated"
    );
    assert_eq!(
        paid.load(Ordering::SeqCst),
        1,
        "call two paid via the callback"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the wallet handler was not consulted for the gated call"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// The reconnect-downgrade shape: this SDK's client transport is not
/// restartable, so "reconnecting with transparent" is a SECOND transport for
/// the same keys (the same identity on the wire). The server serves the
/// downgraded session through the transparent lifecycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downgrade_on_reconnect_with_a_second_transport() {
    let paid = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(50),
        ClientPaymentsOptions::new()
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_on_payment_required(paying_callback(1100, Arc::clone(&paid))),
        |c| c,
        |c| c,
    )
    .await;

    // The gating session does one full paid call.
    fx.client.send(&paid_call("rec-1")).await.expect("send");
    let incoming = tokio::time::timeout(Duration::from_secs(6), fx.server_rx.recv())
        .await
        .expect("the gated call completes")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("rec-1"))
        .await
        .expect("respond");
    let _ = recv_within(&mut fx.client_rx, 3000, "the first result").await;

    // The "reconnect": a second transport under the SAME keys, requesting
    // transparent, with a wallet handler.
    let calls = Arc::new(AtomicUsize::new(0));
    let pool2 = Arc::new(fx.client_pool.linked_with_keys(fx.client_pool.mock_keys()));
    let mut client2 = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(fx.server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_timeout(Duration::from_secs(30)),
        as_pool(&pool2),
    )
    .await
    .expect("second client transport");
    with_client_payments(
        &mut client2,
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))])
            .with_payment_interaction(PaymentInteractionMode::Transparent),
    )
    .expect("register payments on the second transport");
    let mut client2_rx = client2.take_message_receiver().expect("client2 rx");
    client2.start().await.expect("start client2");
    tokio::time::sleep(Duration::from_millis(20)).await;

    client2.send(&paid_call("rec-2")).await.expect("send");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the downgraded session settles transparently")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("rec-2"))
        .await
        .expect("respond");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the downgraded session's result delivers"
        );
        if let JsonRpcMessage::Response(r) =
            recv_within(&mut client2_rx, 3000, "second-session traffic").await
        {
            assert_eq!(r.id, serde_json::json!("rec-2"));
            break;
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the second session paid transparently"
    );
    assert_eq!(
        paid.load(Ordering::SeqCst),
        1,
        "the gating callback ran only for session one"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client one");
    client2.close().await.expect("close client two");
}

/// A CEP-22-fragmented priced call: the raw-request cache is written at
/// `send` (above both publish paths), the invoice correlates to the END
/// frame, the wallet pays, and the result delivers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_priced_call_pays_and_delivers() {
    use contextvm_sdk::transport::oversized_transfer::OversizedTransferConfig;

    let calls = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(200),
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))]),
        |c| c,
        |c| c.with_oversized_transfer(OversizedTransferConfig::default().with_threshold(2000)),
    )
    .await;

    // A priced call big enough to fragment under the tiny client threshold.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("big-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "paid-tool",
            "blob": "X".repeat(4000),
            "_meta": { "progressToken": 5 },
        })),
    });
    fx.client.send(&request).await.expect("send oversized");

    // The transfer really fragmented.
    let frames = fx
        .pool
        .stored_events()
        .await
        .into_iter()
        .filter(|e| e.pubkey == fx.client_pubkey && e.content.contains("oversized-transfer"))
        .count();
    assert!(frames >= 3, "start, chunks, and end frames, saw {frames}");

    let incoming = tokio::time::timeout(Duration::from_secs(5), fx.server_rx.recv())
        .await
        .expect("the reassembled paid request reaches the handler")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("big-1"))
        .await
        .expect("respond");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the oversized paid call's result delivers"
        );
        if let JsonRpcMessage::Response(r) =
            recv_within(&mut fx.client_rx, 5000, "lifecycle traffic").await
        {
            assert_eq!(r.id, serde_json::json!("big-1"));
            break;
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the wallet paid once");

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// The out-of-band shape: a client with NO handlers advertises no PMIs and
/// pays nothing in-band. The invoice reaches the application, the application
/// settles externally (the fake processor's timed settlement stands in for
/// that), and the result delivers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transparent_client_against_optional_server_and_out_of_band_shape() {
    let mut fx = fixture(
        server_options(400),
        ClientPaymentsOptions::new(),
        |c| c,
        |c| c,
    )
    .await;

    fx.client
        .send(&paid_streaming_call("oob-1", 3))
        .await
        .expect("send");

    // No PMI advertisement on the wire.
    let request = client_request_events(&fx.pool, fx.client_pubkey, "oob-1").await[0].clone();
    assert!(
        !all_tags(&request)
            .iter()
            .any(|t| t.first().map(String::as_str) == Some("pmi")),
        "a handler-less client advertises no PMIs"
    );

    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the externally-settled request reaches the handler")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("oob-1"))
        .await
        .expect("respond");

    let mut saw_invoice = false;
    let mut saw_acceptance = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the out-of-band flow delivers"
        );
        match recv_within(&mut fx.client_rx, 3000, "lifecycle traffic").await {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!("oob-1"));
                break;
            }
            JsonRpcMessage::Notification(n) => match n.method.as_str() {
                "notifications/payment_required" => saw_invoice = true,
                "notifications/payment_accepted" => saw_acceptance = true,
                _ => {}
            },
            other => panic!("unexpected client-channel message {other:?}"),
        }
    }
    assert!(saw_invoice, "the invoice reaches the application untouched");
    assert!(
        saw_acceptance,
        "the acceptance follows the external settlement"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

// ── rejection, decline, fresh invoices, canonical identity ──────────────────

/// Rejection and decline surfaces: a server-side pricing rejection is
/// synthesized to the caller with the server's own message (the rejection
/// notification itself is replaced), and a client policy decline surfaces the
/// full decline object carrying the dynamically quoted amount.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejection_and_decline_surfaces() {
    use contextvm_sdk::payments::{ResolvePrice, ResolvePriceParams, ResolvePriceResult};

    struct Rejecting;
    #[async_trait]
    impl ResolvePrice for Rejecting {
        async fn resolve_price(
            &self,
            _params: ResolvePriceParams,
        ) -> Result<ResolvePriceResult, PaymentError> {
            Ok(ResolvePriceResult::Reject {
                message: Some("too rich".to_string()),
            })
        }
    }

    // Face one: the server rejects at pricing time; the client synthesizes the
    // failure onto the caller's own request id and swallows the notification.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(50).with_resolve_price(Arc::new(Rejecting)),
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))]),
        |c| c,
        |c| c,
    )
    .await;
    fx.client.send(&paid_call("rej-1")).await.expect("send");
    let surfaced = recv_within(&mut fx.client_rx, 3000, "the synthesized rejection").await;
    match surfaced {
        JsonRpcMessage::ErrorResponse(e) => {
            assert_eq!(e.id, serde_json::json!("rej-1"));
            assert_eq!(e.error.code, -32000);
            assert_eq!(e.error.message, "Payment rejected: too rich");
            assert!(e.error.data.is_none());
        }
        other => panic!("expected the synthesized rejection, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        fx.client_rx.try_recv().is_err(),
        "the rejection notification itself must not be forwarded"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing was paid");
    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");

    struct Quoting;
    #[async_trait]
    impl ResolvePrice for Quoting {
        async fn resolve_price(
            &self,
            _params: ResolvePriceParams,
        ) -> Result<ResolvePriceResult, PaymentError> {
            Ok(ResolvePriceResult::Quote {
                amount: 42,
                description: None,
                meta: None,
            })
        }
    }

    // Face two: a dynamic quote rides the offer, and a client policy decline
    // surfaces the whole decline object carrying that quoted amount.
    let declining_policy: Arc<PaymentPolicyFn> =
        Arc::new(|_req: PaymentHandlerRequest| async move { false }.boxed());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(50).with_resolve_price(Arc::new(Quoting)),
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))])
            .with_payment_policy(declining_policy),
        |c| c,
        |c| c,
    )
    .await;
    fx.client.send(&paid_call("dec-1")).await.expect("send");
    let invoice = recv_within(&mut fx.client_rx, 3000, "the quoted invoice").await;
    match invoice {
        JsonRpcMessage::Notification(n) => {
            assert_eq!(n.method, "notifications/payment_required");
            assert_eq!(
                n.params.expect("invoice params").get("amount"),
                Some(&serde_json::json!(42)),
                "the dynamically quoted amount rides the offer"
            );
        }
        other => panic!("expected the invoice, got {other:?}"),
    }
    let decline = recv_within(&mut fx.client_rx, 3000, "the policy decline").await;
    match decline {
        JsonRpcMessage::ErrorResponse(e) => {
            assert_eq!(e.id, serde_json::json!("dec-1"));
            assert_eq!(e.error.code, -32000);
            assert_eq!(e.error.message, "Payment declined by client policy");
            assert_eq!(
                e.error.data,
                Some(serde_json::json!({
                    "pmi": "fake",
                    "amount": 42,
                    "method": "tools/call",
                    "capability": "paid-tool",
                })),
                "the decline carries the quoted amount and the original context"
            );
        }
        other => panic!("expected the decline, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the wallet never ran");
    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// When verification fails after the client paid, the server clears its
/// pending state and the client's retry draws a FRESH offer with a new
/// payment request; the callback pays again and the call delivers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_failure_mints_a_fresh_invoice_on_retry() {
    use contextvm_sdk::payments::types::{
        PaymentProcessorCreateParams, PaymentProcessorVerifyParams, PaymentRequiredParams,
        VerifyOutcome,
    };
    use contextvm_sdk::payments::PaymentProcessor;

    /// Fails the first verification, settles from the second on, and mints a
    /// distinct payment request per offer.
    struct FlakyProcessor {
        creates: AtomicUsize,
        verifies: AtomicUsize,
    }
    #[async_trait]
    impl PaymentProcessor for FlakyProcessor {
        fn pmi(&self) -> &str {
            "fake"
        }
        async fn create_payment_required(
            &self,
            p: PaymentProcessorCreateParams,
        ) -> Result<PaymentRequiredParams, PaymentError> {
            let n = self.creates.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(PaymentRequiredParams {
                amount: p.amount,
                pay_req: format!("flaky-invoice-{n}"),
                pmi: "fake".to_string(),
                description: None,
                ttl: None,
                meta: None,
            })
        }
        async fn verify_payment(
            &self,
            _p: PaymentProcessorVerifyParams,
        ) -> Result<VerifyOutcome, PaymentError> {
            let n = self.verifies.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            if n == 0 {
                Err(PaymentError::Processor("verification failed".to_string()))
            } else {
                Ok(VerifyOutcome::default())
            }
        }
    }

    let paid = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        ServerPaymentsOptions::new(
            vec![Arc::new(FlakyProcessor {
                creates: AtomicUsize::new(0),
                verifies: AtomicUsize::new(0),
            })],
            vec![priced_tool(21)],
        ),
        ClientPaymentsOptions::new()
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_on_payment_required(paying_callback(1100, Arc::clone(&paid))),
        |c| c,
        |c| c,
    )
    .await;

    fx.client.send(&paid_call("pay-f1")).await.expect("send");

    let incoming = tokio::time::timeout(Duration::from_secs(10), fx.server_rx.recv())
        .await
        .expect("the second payment round claims and forwards")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("pay-f1"))
        .await
        .expect("respond");
    let result = recv_within(&mut fx.client_rx, 3000, "the result").await;
    match result {
        JsonRpcMessage::Response(r) => assert_eq!(r.id, serde_json::json!("pay-f1")),
        other => panic!("expected the result, got {other:?}"),
    }

    // Two distinct offers with two distinct payment requests were played.
    let offers = server_events_containing(&fx.pool, fx.server_pubkey, "\"code\":-32042").await;
    assert_eq!(
        offers.len(),
        2,
        "the failed verification mints a fresh offer"
    );
    let pay_reqs: Vec<String> = offers
        .iter()
        .map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.content).expect("offer json");
            v["error"]["data"]["payment_options"][0]["pay_req"]
                .as_str()
                .expect("pay_req")
                .to_string()
        })
        .collect();
    assert_ne!(
        pay_reqs[0], pay_reqs[1],
        "each offer carries a fresh invoice"
    );
    assert_eq!(
        paid.load(Ordering::SeqCst),
        2,
        "the callback paid each round"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

/// Canonical identity excludes `_meta`: after a declined-then-settled offer
/// mints a grant, an identical call differing only in its progress token
/// claims it, with exactly one offer and one tool run in total.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_call_with_a_new_progress_token_claims_the_grant() {
    let mut fx = fixture(
        server_options(50),
        ClientPaymentsOptions::new()
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_on_payment_required(declining_callback("not now")),
        |c| c,
        |c| c,
    )
    .await;

    let call = |token: i64| {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("idn-1"),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "paid-tool",
                "_meta": { "progressToken": token },
            })),
        })
    };

    // Call one: the callback declines, so the synthesized refusal surfaces,
    // but the fake settlement still mints a grant server-side (the payment
    // raced the decline; the grant survives for a matching retry).
    fx.client.send(&call(1)).await.expect("send call one");
    let surfaced = recv_within(&mut fx.client_rx, 3000, "the declined offer").await;
    assert!(
        matches!(surfaced, JsonRpcMessage::ErrorResponse(_)),
        "call one surfaces the decline"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Call two differs ONLY in `_meta.progressToken`; canonical identity
    // ignores `_meta`, so it claims call one's grant without a fresh offer.
    fx.client.send(&call(2)).await.expect("send call two");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the identity-matched call claims the grant")
        .expect("server channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("idn-1"))
        .await
        .expect("respond");
    let result = recv_within(&mut fx.client_rx, 3000, "the result").await;
    match result {
        JsonRpcMessage::Response(r) => assert_eq!(r.id, serde_json::json!("idn-1")),
        other => panic!("expected the result, got {other:?}"),
    }
    assert_eq!(
        server_events_containing(&fx.pool, fx.server_pubkey, "\"code\":-32042")
            .await
            .len(),
        1,
        "one offer in total: the grant was claimed, not re-invoiced"
    );
    assert!(fx.server_rx.try_recv().is_err(), "one tool run in total");

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}

// ── the oversized terminal path ─────────────────────────────────────────────

/// Terminal outcomes that ride CEP-22 reassembly hit the SAME terminal hooks
/// as single-event ones: a paid call whose big result arrives fragmented
/// stops its heartbeat on delivery, and a gating error delivered through
/// reassembled frames is intercepted rather than surfacing raw. (Cache
/// retirement through the terminal hooks is pinned at the unit level; this
/// test's observables are the heartbeat stop and the interception.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_result_stops_the_heartbeat_and_intercepts_gating_errors() {
    use contextvm_sdk::transport::oversized_transfer::{
        build_oversized_frames, OversizedSenderOptions, OversizedTransferConfig,
    };

    // Face one: the paid call's RESULT is oversized. Delivery must stop the
    // synthetic heartbeat exactly as a single-event result would.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut fx = fixture(
        server_options(200),
        ClientPaymentsOptions::new()
            .with_handlers(vec![Arc::new(CountingHandler::new(Arc::clone(&calls)))])
            .with_synthetic_progress_interval(Duration::from_millis(100)),
        |c| c.with_oversized_transfer(OversizedTransferConfig::default().with_threshold(2000)),
        |c| c,
    )
    .await;

    fx.client
        .send(&paid_streaming_call("big-r1", 4))
        .await
        .expect("send");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request reaches the handler")
        .expect("server channel open");
    fx.server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("big-r1"),
                result: serde_json::json!({ "blob": "Y".repeat(4000) }),
            }),
        )
        .await
        .expect("respond big");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let delivered = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fragmented result must reassemble and deliver"
        );
        if let JsonRpcMessage::Response(r) =
            recv_within(&mut fx.client_rx, 5000, "lifecycle traffic").await
        {
            break r;
        }
    };
    assert_eq!(delivered.id, serde_json::json!("big-r1"));
    assert_eq!(
        delivered.result["blob"].as_str().map(str::len),
        Some(4000),
        "the whole reassembled result delivers"
    );
    // The heartbeat stopped with the delivery: after a quiet window longer
    // than the beat interval, no further beats arrive.
    while fx.client_rx.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(
        fx.client_rx.try_recv().is_err(),
        "no beats after the oversized result delivered"
    );
    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");

    // Face two: a gating error DELIVERED THROUGH CEP-22 FRAMES is classified
    // and intercepted; the raw offer never reaches the consumer. The frames
    // are fabricated under the server's own keys (the production server
    // answers gating errors single-event; the client must be indifferent).
    let mut fx = fixture(
        server_options(50),
        ClientPaymentsOptions::new()
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_on_payment_required(declining_callback("no thanks")),
        |c| c,
        |c| c,
    )
    .await;

    // A live pending request the server will not answer (unpriced; the
    // response is withheld), so the fabricated error has an entry to consume.
    fx.client.send(&free_call("free-x")).await.expect("send");
    let free_incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the free call reaches the handler")
        .expect("server channel open");

    let error = contextvm_sdk::JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("free-x"),
        error: contextvm_sdk::JsonRpcError {
            code: -32042,
            message: "Payment Required".to_string(),
            data: Some(serde_json::json!({
                "payment_options": [
                    { "amount": 21, "pmi": "fake", "pay_req": "oversized-invoice" }
                ]
            })),
        },
    };
    let serialized =
        serde_json::to_string(&JsonRpcMessage::ErrorResponse(error)).expect("serialize");
    let frames = build_oversized_frames(
        &serialized,
        &OversizedSenderOptions::new("tb-frames").with_chunk_size(serialized.len().div_ceil(2)),
    )
    .expect("build frames");
    let server_base = BaseTransport {
        relay_pool: as_pool(&fx.pool),
        encryption_mode: EncryptionMode::Disabled,
        is_connected: true,
    };
    let request_event_id = EventId::from_hex(&free_incoming.event_id).expect("event id");
    let tags = BaseTransport::create_response_tags(&fx.client_pubkey, &request_event_id);
    for frame in frames.into_ordered() {
        server_base
            .send_mcp_message(
                &JsonRpcMessage::Notification(frame),
                &fx.client_pubkey,
                CTXVM_MESSAGES_KIND,
                tags.clone(),
                Some(false),
                None,
            )
            .await
            .expect("publish frame");
    }

    // The interception outcome (the declined synthesized error) surfaces; the
    // RAW reassembled offer never does. Stripped progress forwards from the
    // transfer may precede it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let surfaced = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the intercepted outcome must surface"
        );
        match recv_within(&mut fx.client_rx, 3000, "transfer traffic").await {
            JsonRpcMessage::ErrorResponse(e) => break e,
            JsonRpcMessage::Notification(_) => continue,
            other => panic!("unexpected client-channel message {other:?}"),
        }
    };
    assert_eq!(surfaced.id, serde_json::json!("free-x"));
    assert_eq!(surfaced.error.code, -32042);
    assert_eq!(
        surfaced.error.data,
        Some(serde_json::json!({ "reason": "no thanks" })),
        "the consumer sees the synthesized refusal, never the raw offer"
    );

    fx.server.close().await.expect("close");
    fx.client.close().await.expect("close client");
}
