//! CEP-8 transparent payment lifecycle, end to end over `MockRelayPool`.
//!
//! A real server transport with the payment middleware registered before `start()`, a
//! real client (a client transport or raw signed events, per test), and every emission
//! asserted on the wire: published events and their tags, never a sender spy.
//!
//! Clock discipline: the stale-route sweep and session expiry run on
//! `std::time::Instant`, which paused tokio time does not advance, so every test here
//! runs on the real clock with tiny configured timeouts. The test clients raise their
//! own timeout above the payment duration: client-side survival of a long payment is a
//! separate concern (the client transport sweeps its own pending store), and these
//! tests must not appear to prove it.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use contextvm_sdk::core::constants::CTXVM_MESSAGES_KIND;
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::payments::fakes::{FakePaymentProcessor, FakePaymentProcessorOptions};
use contextvm_sdk::payments::tags::payment_interaction_tag;
use contextvm_sdk::payments::types::PricedCapability;
use contextvm_sdk::payments::{
    create_server_payments_middleware, ServerPaymentsMiddlewareParams, ServerPaymentsOptions,
};
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::base::BaseTransport;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::open_stream::OpenStreamConfig;
use contextvm_sdk::transport::server::{
    InboundContext, InboundMiddleware, IncomingRequest, Next, NostrServerTransport,
    NostrServerTransportConfig,
};
use contextvm_sdk::{
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, PaymentInteractionMode, RelayPoolTrait,
};
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

fn paid_streaming_call(id: &str, token: &str) -> JsonRpcMessage {
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

/// Poll for one server event containing `needle`, within `deadline`.
async fn wait_for_server_event(
    pool: &Arc<MockRelayPool>,
    server: PublicKey,
    needle: &str,
    deadline: Duration,
) -> Event {
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let found = server_events_containing(pool, server, needle).await;
        if let Some(event) = found.into_iter().next() {
            return event;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "no server event containing {needle:?} within {deadline:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Register the transparent payment middleware on `server` with one fake processor.
fn register_payments(server: &mut NostrServerTransport, verify_delay_ms: u64, amount: i64) {
    let sender = server.payment_notification_sender(Duration::from_secs(300));
    let options = ServerPaymentsOptions::new(
        vec![Arc::new(FakePaymentProcessor::with_options(
            FakePaymentProcessorOptions {
                pmi: "fake".to_string(),
                verify_delay_ms,
                create_delay_ms: 0,
                ttl: None,
            },
        ))],
        vec![priced_tool(amount)],
    );
    server.add_inbound_middleware(create_server_payments_middleware(
        ServerPaymentsMiddlewareParams::new(options, sender),
    ));
}

struct Fx {
    server: NostrServerTransport,
    server_rx: tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
    client: NostrClientTransport,
    client_rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    pool: Arc<MockRelayPool>,
    client_pubkey: PublicKey,
    server_pubkey: PublicKey,
}

/// A paired client/server with the payment middleware registered before `start()`.
/// The client timeout is raised above every payment duration used here (see the module
/// doc for why).
async fn fixture(
    verify_delay_ms: u64,
    configure_config: impl FnOnce(NostrServerTransportConfig) -> NostrServerTransportConfig,
    encryption: EncryptionMode,
) -> Fx {
    fixture_modes(verify_delay_ms, configure_config, encryption, encryption).await
}

/// Like [`fixture`], with independent server and client encryption modes (an encrypted
/// client on an `Optional` server is the one configuration where a per-message
/// encryption flag decides the wire, rather than the transport mode overriding it).
async fn fixture_modes(
    verify_delay_ms: u64,
    configure_config: impl FnOnce(NostrServerTransportConfig) -> NostrServerTransportConfig,
    server_encryption: EncryptionMode,
    client_encryption: EncryptionMode,
) -> Fx {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        configure_config(
            NostrServerTransportConfig::default().with_encryption_mode(server_encryption),
        ),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    register_payments(&mut server, verify_delay_ms, 21);

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(client_encryption)
            .with_timeout(Duration::from_secs(30)),
        Arc::new(client_pool),
    )
    .await
    .expect("client transport");

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
        client_pubkey,
        server_pubkey,
    }
}

/// The client request event carrying `id`, as stored on the shared mock network.
async fn client_request_event(pool: &Arc<MockRelayPool>, client: PublicKey, id: &str) -> Event {
    let needle = format!("\"{id}\"");
    pool.stored_events()
        .await
        .into_iter()
        .find(|e| {
            e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)
                && e.pubkey == client
                && e.content.contains(&needle)
        })
        .unwrap_or_else(|| panic!("client request {id} missing from the relay store"))
}

// ── sweep survival ──────────────────────────────────────────────────────────

/// The core sweep-survival property: a payment that outlives the stale-route sweep
/// still delivers its result, with the original request id and the correct `e` tag.
/// Open-stream stays disabled here, so the delivery is attributable to the payment
/// snapshot fallback alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payment_outliving_the_route_sweep_still_delivers_its_result() {
    let mut fx = fixture(
        400, // settles well after the 100 ms route TTL below
        |c| {
            c.with_request_timeout(Duration::from_millis(100))
                .with_cleanup_interval(Duration::from_millis(50))
        },
        EncryptionMode::Disabled,
    )
    .await;

    fx.client.send(&paid_call("pay-1")).await.expect("send");
    let request_event = client_request_event(&fx.pool, fx.client_pubkey, "pay-1").await;
    let request_event_id = request_event.id.to_hex();

    // The invoice goes out while the route is fresh.
    let required = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;
    assert!(
        all_tags(&required).contains(&vec!["e".to_string(), request_event_id.clone()]),
        "payment_required must correlate to the request event"
    );

    // The request reaches the handler only after settlement.
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the handler")
        .expect("server channel open");
    assert_eq!(incoming.event_id, request_event_id);

    // The route must be genuinely gone before the response is sent, so the delivery
    // below is attributable to the snapshot path, not the ordinary one.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while fx.server.has_event_route(&request_event_id).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the stale-route sweep must have reaped the route during the payment"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    fx.server
        .send_response(&request_event_id, result_response("pay-1"))
        .await
        .expect("the swept-route response must deliver from the snapshot");

    let response = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "\"content\"",
        Duration::from_secs(2),
    )
    .await;
    let tags = all_tags(&response);
    assert!(
        tags.contains(&vec!["e".to_string(), request_event_id.clone()]),
        "the delivered response must carry the request's e tag, got {tags:?}"
    );
    assert!(
        tags.contains(&vec!["p".to_string(), fx.client_pubkey.to_hex()]),
        "the delivered response must address the client"
    );
    assert!(
        response.content.contains("\"pay-1\""),
        "the delivered response must restore the client's own request id"
    );
    let accepted = server_events_containing(&fx.pool, fx.server_pubkey, "payment_accepted").await;
    assert_eq!(accepted.len(), 1, "settlement was acknowledged");

    fx.server.close().await.expect("close");
}

/// A priced `tools/call` that carries a progressToken but never streams lands in the
/// open-stream deferral's passthrough branch with its writer slot deleted; the payment
/// snapshot at the route miss is then the only delivery path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paid_stream_request_that_never_streams_still_delivers() {
    let mut fx = fixture(
        400,
        |c| {
            c.with_request_timeout(Duration::from_millis(100))
                .with_cleanup_interval(Duration::from_millis(50))
                .with_open_stream(OpenStreamConfig::default().with_enabled(true))
        },
        EncryptionMode::Disabled,
    )
    .await;

    fx.client
        .send(&paid_streaming_call("stream-none", "tok-16"))
        .await
        .expect("send");
    let request_event = client_request_event(&fx.pool, fx.client_pubkey, "stream-none").await;
    let request_event_id = request_event.id.to_hex();

    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the handler")
        .expect("server channel open");
    assert_eq!(incoming.event_id, request_event_id);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while fx.server.has_event_route(&request_event_id).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "route must be swept"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The tool "returns" without ever touching its writer.
    fx.server
        .send_response(&request_event_id, result_response("stream-none"))
        .await
        .expect("the never-streamed paid response must deliver");

    let response = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "\"content\"",
        Duration::from_secs(2),
    )
    .await;
    assert!(response.content.contains("\"stream-none\""));
    assert!(all_tags(&response).contains(&vec!["e".to_string(), request_event_id]));

    fx.server.close().await.expect("close");
}

// ── notification shape and disclosure ───────────────────────────────────────

/// `payment_required` carries the correlating `e` tag and never the effective-mode
/// disclosure; the disclosure still rides the next *response*. The latch state is
/// pinned: no server info and no availability advertisement are configured, so the
/// notification's expected tag list is exactly recipient plus correlation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_required_carries_the_e_tag_and_no_disclosure() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let pool = Arc::new(server_pool);
    let client_pool = Arc::new(client_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    register_payments(&mut server, 50, 21);
    let mut server_rx = server.take_message_receiver().expect("rx");
    server.start().await.expect("start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    let client_keys = Keys::generate();
    let client_pubkey = client_keys.public_key();

    // A notification carrying the gating request to a server that does not support it:
    // the session downgrades to transparent with the disclosure armed (a request would
    // have drawn an invalid-params error instead; a notification cannot).
    let init_notif = JsonRpcMessage::Notification(contextvm_sdk::JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/initialized".to_string(),
        params: None,
    });
    let mut tags = BaseTransport::create_recipient_tags(&server_pubkey);
    tags.push(payment_interaction_tag(
        PaymentInteractionMode::ExplicitGating,
    ));
    let notif_event = contextvm_sdk::core::serializers::mcp_to_nostr_event(
        &init_notif,
        CTXVM_MESSAGES_KIND,
        tags,
    )
    .expect("serialize")
    .sign_with_keys(&client_keys)
    .expect("sign");
    client_pool
        .publish_event(&notif_event)
        .await
        .expect("publish");
    tokio::time::sleep(Duration::from_millis(30)).await;

    // The priced request itself carries no negotiation tags.
    let call_event = contextvm_sdk::core::serializers::mcp_to_nostr_event(
        &paid_call("disc-1"),
        CTXVM_MESSAGES_KIND,
        BaseTransport::create_recipient_tags(&server_pubkey),
    )
    .expect("serialize")
    .sign_with_keys(&client_keys)
    .expect("sign");
    let request_event_id = call_event.id.to_hex();
    client_pool
        .publish_event(&call_event)
        .await
        .expect("publish");

    let required = wait_for_server_event(
        &pool,
        server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;
    // The FULL tag list, latch state pinned: this is the session's first outbound
    // event, so the one-shot discovery replay rides it, and with no server info and no
    // availability advertisement configured that replay is exactly the internal
    // oversized-support capability tag. In particular there is NO payment_interaction
    // tag: a notification discharges no disclosure obligation.
    assert_eq!(
        all_tags(&required),
        vec![
            vec!["p".to_string(), client_pubkey.to_hex()],
            vec!["e".to_string(), request_event_id.clone()],
            vec!["support_oversized_transfer".to_string()],
        ],
        "payment_required must carry exactly recipient, correlation, and the discovery replay"
    );

    // Drain the initialized notification and receive the paid request.
    let incoming = loop {
        let msg = tokio::time::timeout(Duration::from_secs(3), server_rx.recv())
            .await
            .expect("the paid request must reach the handler")
            .expect("channel open");
        if matches!(msg.message, JsonRpcMessage::Request(_)) {
            break msg;
        }
    };
    server
        .send_response(&incoming.event_id, result_response("disc-1"))
        .await
        .expect("respond");

    let response =
        wait_for_server_event(&pool, server_pubkey, "\"content\"", Duration::from_secs(2)).await;
    assert!(
        all_tags(&response).contains(&vec![
            "payment_interaction".to_string(),
            "transparent".to_string()
        ]),
        "the next response still owes and carries the effective-mode disclosure, got {:?}",
        all_tags(&response)
    );

    server.close().await.expect("close");
}

// ── duplicates and concurrency ──────────────────────────────────────────────

/// A redelivered request event while the payment is in flight: the duplicate joins the
/// in-flight payment, its chain run pops the route, and the result is still delivered
/// from the snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redelivered_request_event_still_gets_its_result() {
    let mut fx = fixture(
        400,
        |c| c.with_cleanup_interval(Duration::from_millis(50)),
        EncryptionMode::Disabled,
    )
    .await;

    fx.client.send(&paid_call("dup-1")).await.expect("send");
    let request_event = client_request_event(&fx.pool, fx.client_pubkey, "dup-1").await;
    let request_event_id = request_event.id.to_hex();

    // Redeliver the identical event while the payment is in flight.
    tokio::time::sleep(Duration::from_millis(100)).await;
    fx.pool
        .publish_event(&request_event)
        .await
        .expect("redeliver");

    // Exactly one delivery reaches the handler.
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the handler")
        .expect("channel open");
    assert_eq!(incoming.event_id, request_event_id);

    // One charge on the wire.
    let required = server_events_containing(&fx.pool, fx.server_pubkey, "payment_required").await;
    assert_eq!(required.len(), 1, "the duplicate must not re-invoice");

    // The duplicate's chain run pops the delivered request's route.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while fx.server.has_event_route(&request_event_id).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the duplicate's cleanup must pop the route"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    fx.server
        .send_response(&request_event_id, result_response("dup-1"))
        .await
        .expect("the response must deliver from the snapshot after the duplicate popped the route");

    let response = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "\"content\"",
        Duration::from_secs(2),
    )
    .await;
    assert!(response.content.contains("\"dup-1\""));
    assert!(all_tags(&response).contains(&vec!["e".to_string(), request_event_id]));

    fx.server.close().await.expect("close");
}

/// Two priced requests from one client overlap in wall time: the positive control for
/// the no-guard-across-await rule. An implementation that holds the pending-cache lock
/// across the verify serializes them and fails the bound below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_priced_requests_from_one_client_overlap_in_wall_time() {
    let mut fx = fixture(400, |c| c, EncryptionMode::Disabled).await;

    let started = tokio::time::Instant::now();
    fx.client.send(&paid_call("par-1")).await.expect("send 1");
    fx.client.send(&paid_call("par-2")).await.expect("send 2");

    let first = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("first paid request answered")
        .expect("channel open");
    let second = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("second paid request answered")
        .expect("channel open");
    let elapsed = started.elapsed();

    assert_ne!(first.event_id, second.event_id);
    assert!(
        elapsed < Duration::from_millis(700),
        "two 400 ms payments must overlap, not serialize; took {elapsed:?}"
    );

    fx.server.close().await.expect("close");
}

// ── shutdown ────────────────────────────────────────────────────────────────

/// Closing the transport aborts an in-flight verification: the processor observes the
/// cancellation promptly and nothing is forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_transport_aborts_an_in_flight_verify() {
    let mut fx = fixture(10_000, |c| c, EncryptionMode::Disabled).await;

    fx.client.send(&paid_call("shut-1")).await.expect("send");
    // The invoice is out: the lifecycle is parked in the 10 s verify.
    wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;

    let close_started = tokio::time::Instant::now();
    fx.server.close().await.expect("close");
    assert!(
        close_started.elapsed() < Duration::from_secs(5),
        "close must not wait out a 10 s verify"
    );

    // The cancelled verify fails the lifecycle: nothing was (or will be) forwarded.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        fx.server_rx.try_recv().is_err(),
        "a request whose verify was aborted by shutdown must never reach the handler"
    );
    let accepted = server_events_containing(&fx.pool, fx.server_pubkey, "payment_accepted").await;
    assert!(accepted.is_empty(), "no settlement was acknowledged");
}

// ── the oversized re-inject dispatch site ───────────────────────────────────

/// One recorded dispatch: the correlation event id and the context's client PMIs.
type RecordedCtx = (String, Option<Vec<String>>);

/// Records the inbound context the (reassembled) request is dispatched with.
#[derive(Clone, Default)]
struct CtxRecorder(Arc<StdMutex<Vec<RecordedCtx>>>);

#[async_trait]
impl InboundMiddleware for CtxRecorder {
    async fn handle(&self, message: JsonRpcMessage, ctx: &InboundContext, next: Next) -> bool {
        if matches!(message, JsonRpcMessage::Request(_)) {
            self.0
                .lock()
                .unwrap()
                .push((ctx.request_event_id.clone(), ctx.client_pmis.clone()));
        }
        next.run(message).await
    }
}

/// The oversized re-inject path is the second dispatch site, and it is gated exactly
/// like the primary one. The oversized send is the client's FIRST send (a small control
/// request first would spend the discovery tags, suppress the server's accept, and
/// abort the transfer), it carries a progressToken (nothing fragments without one), and
/// the re-injected context presents no client PMIs even though the client advertises
/// them (they ride the start frame, not the end frame the server keys identity from).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_priced_request_is_gated_on_the_re_inject_path() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    let recorder = CtxRecorder::default();
    server.add_inbound_middleware(Arc::new(recorder.clone()));
    register_payments(&mut server, 50, 21);
    let mut server_rx = server.take_message_receiver().expect("rx");
    server.start().await.expect("start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_pmis(vec!["fake".to_string()])
            .with_timeout(Duration::from_secs(30)),
        Arc::new(client_pool),
    )
    .await
    .expect("client transport");
    client.start().await.expect("client start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // An oversized priced request as the FIRST send, with a progressToken.
    let blob = "x".repeat(200_000);
    let oversized = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("big-1"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "paid-tool",
            "arguments": { "blob": blob },
            "_meta": { "progressToken": "tok-big" },
        })),
    });
    client.send(&oversized).await.expect("oversized send");

    // The reassembled request is gated: the invoice correlates to the END frame's
    // carrying event, which is also the id the request is dispatched under.
    let incoming = tokio::time::timeout(Duration::from_secs(5), server_rx.recv())
        .await
        .expect("the reassembled paid request must reach the handler")
        .expect("channel open");
    let end_frame_event_id = incoming.event_id.clone();

    let required = wait_for_server_event(
        &pool,
        server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;
    assert!(
        all_tags(&required).contains(&vec!["e".to_string(), end_frame_event_id.clone()]),
        "the invoice must correlate to the end frame's event id"
    );

    // The re-injected context presents no PMIs, even though the client advertised one:
    // they rode the start frame.
    let recorded = recorder.0.lock().unwrap().clone();
    let (recorded_id, recorded_pmis) = recorded
        .iter()
        .find(|(id, _)| *id == end_frame_event_id)
        .expect("the re-injected request must pass through the chain")
        .clone();
    assert_eq!(recorded_id, end_frame_event_id);
    assert_eq!(
        recorded_pmis, None,
        "the end frame carries no pmi tags for this SDK's client"
    );

    server
        .send_response(&end_frame_event_id, result_response("big-1"))
        .await
        .expect("respond");
    let response =
        wait_for_server_event(&pool, server_pubkey, "\"content\"", Duration::from_secs(2)).await;
    assert!(response.content.contains("\"big-1\""));

    server.close().await.expect("close");
}

// ── encryption ──────────────────────────────────────────────────────────────

/// The whole lifecycle under required encryption: the invoice, the acceptance and the
/// result all reach the client through gift wraps, and the server publishes nothing in
/// plaintext.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_session_gates_and_delivers_end_to_end() {
    let mut fx = fixture(50, |c| c, EncryptionMode::Required).await;
    let client_rx = &mut fx.client_rx;

    fx.client.send(&paid_call("enc-1")).await.expect("send");

    // The paid request reaches the handler after settlement; answer it.
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the handler")
        .expect("channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("enc-1"))
        .await
        .expect("respond");

    // The client decrypts the invoice off the gift wrap: the inner-tag correlation and
    // the wrap path work end to end for a payment notification.
    let invoice = tokio::time::timeout(Duration::from_secs(3), client_rx.recv())
        .await
        .expect("the client must decrypt and receive the invoice")
        .expect("client channel open");
    match invoice {
        JsonRpcMessage::Notification(n) => {
            assert_eq!(n.method, "notifications/payment_required");
            let params = n.params.expect("invoice params");
            assert_eq!(params.get("amount").unwrap(), 21);
            assert_eq!(params.get("pmi").unwrap(), "fake");
        }
        other => panic!("expected the invoice first, got {other:?}"),
    }

    // The correlation entry survives the correlated invoice notification, so the
    // acceptance and the paid result also reach the paying client's own channel.
    let accepted = tokio::time::timeout(Duration::from_secs(3), client_rx.recv())
        .await
        .expect("the client must receive the acceptance notification")
        .expect("client channel open");
    match accepted {
        JsonRpcMessage::Notification(n) => {
            assert_eq!(n.method, "notifications/payment_accepted");
        }
        other => panic!("expected the acceptance notification, got {other:?}"),
    }
    let delivered = tokio::time::timeout(Duration::from_secs(3), client_rx.recv())
        .await
        .expect("the client must receive the paid result")
        .expect("client channel open");
    match delivered {
        JsonRpcMessage::Response(r) => {
            assert_eq!(r.id, serde_json::json!("enc-1"));
            assert!(r.result.get("content").is_some(), "the paid result body");
        }
        other => panic!("expected the paid result, got {other:?}"),
    }

    // Everything on the wire rides a gift wrap: no plaintext ContextVM event at all,
    // and at least one wrap per lifecycle emission (the request, the invoice, the
    // acceptance, the result).
    let plaintext: Vec<Event> = fx
        .pool
        .stored_events()
        .await
        .into_iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND))
        .collect();
    assert!(
        plaintext.is_empty(),
        "an encrypted session must never emit plaintext payment traffic"
    );
    let wraps = fx
        .pool
        .stored_events()
        .await
        .into_iter()
        .filter(|e| e.kind == Kind::Custom(1059) || e.kind == Kind::Custom(21059))
        .count();
    assert!(
        wraps >= 4,
        "the request, the invoice, the acceptance and the result must each ride a \
         gift wrap; saw {wraps}"
    );

    fx.server.close().await.expect("close");
}

/// The sweep-survival path for an encrypted client: the snapshot must carry the
/// session's encryption state, or the paid result is published in plaintext after the
/// sweep. The server runs `EncryptionMode::Optional` deliberately: under `Required` the
/// transport mode forces encryption no matter what the snapshot says, so `Optional`
/// plus an encrypted client is the one configuration where the snapshot's own field
/// decides the wire. Every other sweep test runs plaintext, so this is the only
/// coverage of the snapshot's encrypted-delivery fields.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_payment_outliving_the_sweep_still_delivers_encrypted() {
    let mut fx = fixture_modes(
        400,
        |c| {
            c.with_request_timeout(Duration::from_millis(100))
                .with_cleanup_interval(Duration::from_millis(50))
        },
        EncryptionMode::Optional,
        EncryptionMode::Required,
    )
    .await;
    let client_rx = &mut fx.client_rx;

    fx.client
        .send(&paid_call("enc-sweep-1"))
        .await
        .expect("send");

    // Positive control that the encrypted lifecycle is really running: the client
    // decrypts the invoice off its gift wrap.
    let invoice = tokio::time::timeout(Duration::from_secs(3), client_rx.recv())
        .await
        .expect("the client must receive the invoice")
        .expect("client channel open");
    match invoice {
        JsonRpcMessage::Notification(n) => {
            assert_eq!(n.method, "notifications/payment_required")
        }
        other => panic!("expected the invoice, got {other:?}"),
    }

    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the handler")
        .expect("channel open");

    // The route must be genuinely gone, so the delivery below is the snapshot's.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while fx.server.has_event_route(&incoming.event_id).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "route must be swept"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let wraps_before = fx
        .pool
        .stored_events()
        .await
        .into_iter()
        .filter(|e| e.kind == Kind::Custom(1059) || e.kind == Kind::Custom(21059))
        .count();

    fx.server
        .send_response(&incoming.event_id, result_response("enc-sweep-1"))
        .await
        .expect("the swept encrypted response must deliver from the snapshot");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The delivered result rides a gift wrap, and nothing the session produced is
    // plaintext ContextVM.
    let events = fx.pool.stored_events().await;
    let wraps_after = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(1059) || e.kind == Kind::Custom(21059))
        .count();
    assert!(
        wraps_after > wraps_before,
        "the snapshot delivery must publish a gift wrap"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)),
        "an encrypted session's paid result must never be published in plaintext"
    );

    // Delivery, not just publication: the acceptance and the swept-route paid result
    // decrypt and arrive on the paying client's own channel (the correlation entry
    // survives the correlated invoice notification).
    let accepted = tokio::time::timeout(Duration::from_secs(3), client_rx.recv())
        .await
        .expect("the client must receive the acceptance notification")
        .expect("client channel open");
    match accepted {
        JsonRpcMessage::Notification(n) => {
            assert_eq!(n.method, "notifications/payment_accepted");
        }
        other => panic!("expected the acceptance notification, got {other:?}"),
    }
    let delivered = tokio::time::timeout(Duration::from_secs(3), client_rx.recv())
        .await
        .expect("the client must receive the paid result")
        .expect("client channel open");
    match delivered {
        JsonRpcMessage::Response(r) => assert_eq!(r.id, serde_json::json!("enc-sweep-1")),
        other => panic!("expected the paid result, got {other:?}"),
    }

    fx.server.close().await.expect("close");
}

// ── observability ───────────────────────────────────────────────────────────

/// The warn on a route-less snapshot capture is the only signal that a
/// `payment_required` was published for a request whose result can no longer be
/// delivered; pin that it fires.
#[tokio::test]
async fn a_missing_route_at_snapshot_time_warns() {
    use std::io::Write;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Capture(Arc<StdMutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut fx = fixture(50, |c| c, EncryptionMode::Disabled).await;

    // Establish a session (and then answer the request, so its route is gone).
    fx.client.send(&paid_call("warm-1")).await.expect("send");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("request")
        .expect("channel");
    fx.server
        .send_response(&incoming.event_id, result_response("warm-1"))
        .await
        .expect("respond");

    // A payment_required for an event id with no live route: the sender must warn and
    // capture nothing, then still publish.
    let sender = fx
        .server
        .payment_notification_sender(Duration::from_secs(300));
    let orphan = JsonRpcMessage::Notification(contextvm_sdk::JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/payment_required".to_string(),
        params: Some(serde_json::json!({ "amount": 1, "pay_req": "inv", "pmi": "fake" })),
    });
    sender(fx.client_pubkey.to_hex(), "ab".repeat(32), None, orphan)
        .await
        .expect("the publish itself succeeds");

    let logged = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
    assert!(
        logged.contains("route already gone"),
        "the route-less capture must be observable; captured: {logged}"
    );

    fx.server.close().await.expect("close");
}

// ── liveness through the real worker ────────────────────────────────────────

#[cfg(feature = "rmcp")]
mod worker_liveness {
    use super::*;
    use rmcp::model::{
        Implementation, ProtocolVersion, ServerCapabilities, ServerInfo as RmcpServerInfo,
    };
    use rmcp::{ServerHandler, ServiceExt};

    struct LivenessServer;
    impl ServerHandler for LivenessServer {
        fn get_info(&self) -> RmcpServerInfo {
            let mut info =
                RmcpServerInfo::new(ServerCapabilities::builder().enable_tools().build());
            info.protocol_version = ProtocolVersion::LATEST;
            info.server_info = Implementation::new("payments-liveness-server", "0.1.0");
            info
        }
    }

    fn wire_event(keys: &Keys, server: PublicKey, message: &JsonRpcMessage) -> Event {
        contextvm_sdk::core::serializers::mcp_to_nostr_event(
            message,
            CTXVM_MESSAGES_KIND,
            BaseTransport::create_recipient_tags(&server),
        )
        .expect("serialize")
        .sign_with_keys(keys)
        .expect("sign")
    }

    /// A priced request parked in a multi-minute verify must not delay an unpriced
    /// request from the same client reaching the handler and being answered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_held_payment_does_not_stall_other_requests() {
        let (client_pool, server_pool) = MockRelayPool::create_pair();
        let server_pubkey = server_pool.mock_public_key();
        let pool = Arc::new(server_pool);
        let client_pool = Arc::new(client_pool);

        let mut transport = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
            as_pool(&pool),
        )
        .await
        .expect("server transport");
        register_payments(&mut transport, 30_000, 21);

        let server_task = tokio::spawn(async move {
            match LivenessServer.serve(transport).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(error) => panic!("serve failed: {error}"),
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let client_keys = Keys::generate();
        // The priced request parks in a 30 s verify...
        client_pool
            .publish_event(&wire_event(
                &client_keys,
                server_pubkey,
                &paid_call("held-1"),
            ))
            .await
            .expect("publish paid call");
        wait_for_server_event(
            &pool,
            server_pubkey,
            "payment_required",
            Duration::from_secs(3),
        )
        .await;

        // ...while an unpriced request from the same client is answered promptly.
        let free = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("free-1"),
            method: "tools/list".to_string(),
            params: Some(serde_json::json!({})),
        });
        client_pool
            .publish_event(&wire_event(&client_keys, server_pubkey, &free))
            .await
            .expect("publish free call");

        let answered =
            wait_for_server_event(&pool, server_pubkey, "\"free-1\"", Duration::from_secs(3)).await;
        assert!(
            answered.content.contains("\"tools\""),
            "the unpriced request must be answered while the payment is held; got {}",
            answered.content
        );

        server_task.abort();
    }
}
