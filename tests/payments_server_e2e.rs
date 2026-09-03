//! CEP-8 server payments through the production registration entry point, end to end
//! over `MockRelayPool`.
//!
//! Every server in this suite is configured via `with_server_payments`, never by
//! hand-registering middlewares: the suite exists to prove the production wiring (tag
//! composition, policy recording, sender capture order, conditional gating
//! registration, the snapshot TTL threading) drives both payment lifecycles
//! correctly. The factory-level suites keep their hand-registration; this one must
//! not.
//!
//! Clock discipline: the authorization store, the stale-route sweep and session expiry
//! all run on `std::time::Instant`, which paused tokio time does not advance, so every
//! test here runs on the real clock with tiny configured timeouts. A test that mixes
//! the paused tokio clock with a store-TTL assertion proves nothing while green, which
//! is why none does.

use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::core::constants::CTXVM_MESSAGES_KIND;
use contextvm_sdk::core::serializers;
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::payments::fakes::{FakePaymentProcessor, FakePaymentProcessorOptions};
use contextvm_sdk::payments::types::PricedCapability;
use contextvm_sdk::payments::{
    with_server_payments, PaymentInteractionPolicy, ServerPaymentsOptions,
};
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::base::BaseTransport;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{
    IncomingRequest, NostrServerTransport, NostrServerTransportConfig,
};
use contextvm_sdk::{
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, PaymentInteractionMode, RelayPoolTrait,
    ServerInfo,
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

/// The standard one-processor, one-priced-tool payments configuration.
fn payments_options(verify_delay_ms: u64) -> ServerPaymentsOptions {
    ServerPaymentsOptions::new(
        vec![fake_processor("fake", verify_delay_ms)],
        vec![priced_tool(21)],
    )
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

/// Poll `cond` for up to `deadline`.
async fn wait_until(what: &str, deadline: Duration, mut cond: impl AsyncFnMut() -> bool) {
    let end = tokio::time::Instant::now() + deadline;
    loop {
        if cond().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "condition not reached within {deadline:?}: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

struct Fx {
    server: NostrServerTransport,
    server_rx: tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
    client: NostrClientTransport,
    pool: Arc<MockRelayPool>,
    client_pubkey: PublicKey,
    server_pubkey: PublicKey,
}

/// A paired client/server with payments registered through the PRODUCTION entry
/// point before `start()`. The client's mode and PMIs are per test; a `None` mode is
/// the default (transparent) client.
async fn fixture(
    options: ServerPaymentsOptions,
    configure_config: impl FnOnce(NostrServerTransportConfig) -> NostrServerTransportConfig,
    client_mode: Option<PaymentInteractionMode>,
) -> Fx {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        configure_config(
            NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        ),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    with_server_payments(&mut server, options).expect("register payments");

    let mut client_config = NostrClientTransportConfig::default()
        .with_relay_urls(vec!["wss://mock.relay".to_string()])
        .with_server_pubkey(server_pubkey.to_hex())
        .with_encryption_mode(EncryptionMode::Disabled)
        .with_timeout(Duration::from_secs(30));
    if let Some(mode) = client_mode {
        client_config = client_config
            .with_payment_interaction(mode)
            .with_pmis(vec!["fake".to_string()]);
    }
    let mut client = NostrClientTransport::with_relay_pool(client_config, Arc::new(client_pool))
        .await
        .expect("client transport");

    let server_rx = server.take_message_receiver().expect("server rx");
    let _client_rx = client.take_message_receiver().expect("client rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    Fx {
        server,
        server_rx,
        client,
        pool,
        client_pubkey,
        server_pubkey,
    }
}

/// A raw signed `tools/call` for `paid-tool` from `keys`, with optional extra tags
/// (used to place `payment_interaction` upserts on individual messages).
fn signed_paid_call(
    keys: &Keys,
    server_pubkey: PublicKey,
    id: &str,
    extra_tags: Vec<Tag>,
) -> Event {
    let mut tags = BaseTransport::create_recipient_tags(&server_pubkey);
    tags.extend(extra_tags);
    serializers::mcp_to_nostr_event(&paid_call(id), CTXVM_MESSAGES_KIND, tags)
        .expect("serialize the priced call")
        .sign_with_keys(keys)
        .expect("sign the priced call")
}

fn pi_tag(value: &str) -> Tag {
    Tag::custom(
        TagKind::Custom("payment_interaction".into()),
        vec![value.to_string()],
    )
}

// ── the two lifecycles through the entry point ──────────────────────────────

/// A default-mode client's priced call runs the full transparent lifecycle through
/// the production wiring: the invoice carries the discovery replay INCLUDING the
/// payment discovery tags (proof the notification sender was built after the tag
/// setters ran), settlement is acknowledged, and the paid request reaches the
/// handler.
///
/// Fixture rule, load-bearing: the priced call MUST be the session's first message
/// and the replay assertion MUST include the `pmi` tags. The one-shot discovery
/// latch burns on the session's first outbound event regardless of content, and the
/// normal response path reads the tag sets live, so a fixture that lets any response
/// precede the priced call would leave a build-senders-before-tags wiring invisible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transparent_lifecycle_engages_through_the_entry_point() {
    let mut fx = fixture(payments_options(50), |c| c, None).await;

    fx.client.send(&paid_call("pay-t1")).await.expect("send");
    let request_event_id = client_request_events(&fx.pool, fx.client_pubkey, "pay-t1").await[0]
        .id
        .to_hex();

    let required = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;
    // The FULL tag list in order: recipient, correlation, then the one-shot discovery
    // replay (payment discovery tags first, transport-internal capability last).
    assert_eq!(
        all_tags(&required),
        vec![
            vec!["p".to_string(), fx.client_pubkey.to_hex()],
            vec!["e".to_string(), request_event_id.clone()],
            vec!["pmi".to_string(), "fake".to_string()],
            vec![
                "payment_interaction".to_string(),
                "explicit_gating".to_string()
            ],
            vec!["support_oversized_transfer".to_string()],
        ],
        "the invoice must replay the full discovery set captured at registration"
    );

    // The request reaches the handler only after the fake settles.
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the handler")
        .expect("channel open");
    assert_eq!(incoming.event_id, request_event_id);
    let accepted = server_events_containing(&fx.pool, fx.server_pubkey, "payment_accepted").await;
    assert_eq!(accepted.len(), 1, "settlement must be acknowledged");

    fx.server
        .send_response(&request_event_id, result_response("pay-t1"))
        .await
        .expect("respond");
    let response = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "\"content\"",
        Duration::from_secs(2),
    )
    .await;
    assert!(
        all_tags(&response).contains(&vec!["e".to_string(), request_event_id]),
        "the response must correlate to the request"
    );

    fx.server.close().await.expect("close");
}

/// A gating client's first priced call is answered `-32042` with the full first
/// response tag surface, the handler stays silent until the payment settles, and the
/// paid retry claims the grant and delivers the result.
///
/// Fixture rule, load-bearing: the priced call MUST be the session's first message
/// and the offer's tag assertion MUST include the `pmi` tags (see the transparent
/// twin above for why an earlier response would disarm this assert).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gating_lifecycle_engages_through_the_entry_point() {
    let mut fx = fixture(
        payments_options(50),
        |c| c,
        Some(PaymentInteractionMode::ExplicitGating),
    )
    .await;

    fx.client.send(&paid_call("pay-g1")).await.expect("send");
    let request_event_id = client_request_events(&fx.pool, fx.client_pubkey, "pay-g1").await[0]
        .id
        .to_hex();

    let offer = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;
    // The FULL tag list in order: recipient, correlation, the discovery replay
    // including the payment discovery tags, with the effective-mode disclosure
    // deduplicated against the replayed availability advertisement.
    assert_eq!(
        all_tags(&offer),
        vec![
            vec!["p".to_string(), fx.client_pubkey.to_hex()],
            vec!["e".to_string(), request_event_id.clone()],
            vec!["pmi".to_string(), "fake".to_string()],
            vec![
                "payment_interaction".to_string(),
                "explicit_gating".to_string()
            ],
            vec!["support_oversized_transfer".to_string()],
        ],
        "the offer must compose the full first-response tag surface"
    );
    // The payload keeps the client's own inner request id and offers the fake PMI.
    assert!(
        offer.content.contains("\"id\":\"pay-g1\""),
        "the error id must be the original inner request id, got {}",
        offer.content
    );
    assert!(offer.content.contains("\"code\":-32042"));
    assert!(offer.content.contains("\"pmi\":\"fake\""));

    // The gated request never reaches the handler before payment.
    assert!(
        fx.server_rx.try_recv().is_err(),
        "the gated request must not reach the handler unpaid"
    );

    // The fake settles 50 ms after the offer; the margin is generous (real clock).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The retry is a fresh request event with the same method and params: it claims
    // the grant and forwards.
    fx.client.send(&paid_call("pay-g1")).await.expect("retry");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid retry must reach the handler")
        .expect("channel open");
    assert_ne!(
        incoming.event_id, request_event_id,
        "the claiming invocation is a new event"
    );

    fx.server
        .send_response(&incoming.event_id, result_response("pay-g1"))
        .await
        .expect("respond");
    let response = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "\"content\"",
        Duration::from_secs(2),
    )
    .await;
    assert!(
        all_tags(&response).contains(&vec!["e".to_string(), incoming.event_id.clone()]),
        "the result rides the claiming invocation's own correlation"
    );

    fx.server.close().await.expect("close");
}

// ── the transparent-only policy ─────────────────────────────────────────────

/// A transparent-only server rejects the gating request with the whole `-32602`
/// object, still gates the same client's subsequent priced call through the
/// transparent lifecycle, and advertises no `payment_interaction` tag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transparent_policy_rejects_gating_and_still_gates() {
    let mut fx = fixture(
        payments_options(50).with_payment_interaction(PaymentInteractionPolicy::Transparent),
        |c| c.with_server_info(ServerInfo::default().with_name("transparent-only")),
        Some(PaymentInteractionMode::ExplicitGating),
    )
    .await;

    // Request 1: the mode request draws the whole -32602 object.
    fx.client.send(&paid_call("rej-1")).await.expect("send");
    let rejection = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Unsupported payment_interaction",
        Duration::from_secs(2),
    )
    .await;
    let payload: serde_json::Value =
        serde_json::from_str(&rejection.content).expect("rejection parses");
    assert_eq!(
        payload,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "rej-1",
            "error": {
                "code": -32602,
                "message": "Unsupported payment_interaction mode: explicit_gating",
                "data": {
                    "requested": "explicit_gating",
                    "supported": ["transparent"],
                },
            },
        }),
        "the whole rejection object must match the negotiation wire shape"
    );
    assert!(
        fx.server_rx.try_recv().is_err(),
        "the rejected request must not reach the handler"
    );

    // Request 2: the client's one-shot mode latch is spent, so this runs under the
    // default transparent mode, and the priced call is still gated.
    fx.client.send(&paid_call("rej-2")).await.expect("send");
    wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid transparent request must reach the handler")
        .expect("channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("rej-2"))
        .await
        .expect("respond");

    // The announcement advertises the PMIs and prices but never a mode.
    fx.server.announce().await.expect("announce");
    let announcement = fx
        .pool
        .stored_events()
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Custom(11316))
        .expect("announcement");
    assert!(
        !all_tags(&announcement)
            .iter()
            .any(|t| t.first().map(String::as_str) == Some("payment_interaction")),
        "a transparent-only server must not advertise a payment_interaction tag"
    );

    fx.server.close().await.expect("close");
}

// ── the oversized re-inject dispatch site ───────────────────────────────────

/// A CEP-22 reassembled priced call in a gating session is gated on the re-inject
/// dispatch site: the offer correlates to the end frame's event and takes the first
/// configured processor (the re-injected context presents no client PMIs; they rode
/// the start frame), and the paid oversized retry claims and round-trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_reassembled_priced_call_is_gated() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    with_server_payments(&mut server, payments_options(50)).expect("register payments");
    let mut server_rx = server.take_message_receiver().expect("rx");
    server.start().await.expect("start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_pmis(vec!["fake".to_string()])
            .with_timeout(Duration::from_secs(30)),
        Arc::new(client_pool),
    )
    .await
    .expect("client transport");
    client.start().await.expect("client start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // An oversized priced request as the FIRST send, with a progressToken (nothing
    // fragments without one).
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

    let offer = wait_for_server_event(
        &pool,
        server_pubkey,
        "Payment Required",
        Duration::from_secs(5),
    )
    .await;
    assert!(offer.content.contains("\"code\":-32042"));
    assert!(
        offer.content.contains("\"pmi\":\"fake\""),
        "with no PMIs on the re-injected context the offer takes the first processor"
    );
    assert!(
        server_rx.try_recv().is_err(),
        "the gated reassembled request must not reach the handler unpaid"
    );

    // The fake settles 50 ms after the offer; then the oversized retry claims.
    tokio::time::sleep(Duration::from_millis(500)).await;
    client.send(&oversized).await.expect("oversized retry");
    let incoming = tokio::time::timeout(Duration::from_secs(5), server_rx.recv())
        .await
        .expect("the paid reassembled retry must reach the handler")
        .expect("channel open");
    server
        .send_response(&incoming.event_id, result_response("big-1"))
        .await
        .expect("respond");
    let response =
        wait_for_server_event(&pool, server_pubkey, "\"content\"", Duration::from_secs(2)).await;
    assert!(response.content.contains("\"big-1\""));

    server.close().await.expect("close");
}

// ── sweep survival with the threaded snapshot TTL ───────────────────────────

/// A payment that outlives the stale-route sweep still delivers its result through
/// the production wiring: the snapshot recorded at invoice time (with the TTL the
/// entry point threads from the configured `payment_ttl`) is the only delivery path
/// once the route is swept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_payment_survives_the_sweep_with_the_threaded_snapshot_ttl() {
    let mut fx = fixture(
        payments_options(400).with_payment_ttl(Duration::from_secs(120)),
        |c| {
            c.with_request_timeout(Duration::from_millis(100))
                .with_cleanup_interval(Duration::from_millis(50))
        },
        None,
    )
    .await;

    fx.client.send(&paid_call("sweep-1")).await.expect("send");
    let request_event_id = client_request_events(&fx.pool, fx.client_pubkey, "sweep-1").await[0]
        .id
        .to_hex();

    wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;

    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid request must reach the handler")
        .expect("channel open");
    assert_eq!(incoming.event_id, request_event_id);

    // The route must be genuinely gone before the response is sent, so the delivery
    // below is attributable to the snapshot path alone.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while fx.server.has_event_route(&request_event_id).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the stale-route sweep must have reaped the route during the payment"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    fx.server
        .send_response(&request_event_id, result_response("sweep-1"))
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
        "the delivered response must carry the request's correlation, got {tags:?}"
    );
    assert!(
        response.content.contains("\"sweep-1\""),
        "the delivered response must restore the client's own request id"
    );

    fx.server.close().await.expect("close");
}

// ── no authorization migration across lifecycles ────────────────────────────

/// One client public key, one server, the session mode flipped by mid-session
/// `payment_interaction` updates: a completed transparent payment mints no gating
/// state, and a gating grant is neither consumed nor honored by the transparent
/// lifecycle, surviving intact for the gating retry. Wire-observable throughout; no
/// store access. The single-pubkey shape is what makes this falsifiable: grants key
/// on the client public key plus the canonical invocation identity, so a two-client
/// fixture could never observe a cross-lifecycle migration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_do_not_migrate_across_lifecycles() {
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
    with_server_payments(&mut server, payments_options(50)).expect("register payments");
    let mut server_rx = server.take_message_receiver().expect("rx");
    server.start().await.expect("start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // One client keypair for the whole test: raw signed events, so each message
    // controls its own payment_interaction tag (the client transport's one-shot
    // emission latch cannot express a mid-session update).
    let keys = Keys::generate();

    // 1) Under the default transparent mode, complete a paid call.
    let m1 = signed_paid_call(&keys, server_pubkey, "mig-1", vec![]);
    client_pool.publish_event(&m1).await.expect("publish");
    wait_for_server_event(
        &pool,
        server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;
    let incoming = tokio::time::timeout(Duration::from_secs(3), server_rx.recv())
        .await
        .expect("the paid transparent request must reach the handler")
        .expect("channel open");
    assert_eq!(incoming.event_id, m1.id.to_hex());
    server
        .send_response(&incoming.event_id, result_response("mig-1"))
        .await
        .expect("respond");

    // 2) Update the session to explicit gating and re-send the identical
    // invocation: the completed transparent payment must have minted no gating
    // grant, so this is offered, not forwarded.
    let m2 = signed_paid_call(
        &keys,
        server_pubkey,
        "mig-2",
        vec![pi_tag("explicit_gating")],
    );
    client_pool.publish_event(&m2).await.expect("publish");
    let offer = wait_for_server_event(
        &pool,
        server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;
    assert!(
        all_tags(&offer).contains(&vec!["e".to_string(), m2.id.to_hex()]),
        "the offer must answer the gating invocation"
    );
    assert!(offer.content.contains("\"code\":-32042"));
    assert!(
        server_rx.try_recv().is_err(),
        "a transparent execution must never mint an explicit-gating authorization"
    );

    // The fake settles the offered payment 50 ms later: a gating grant now exists.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 3) Update back to transparent WITHOUT retrying: the identical invocation must
    // run the transparent lifecycle (a fresh invoice), not consume the gating grant
    // as a free forward.
    let m3 = signed_paid_call(&keys, server_pubkey, "mig-3", vec![pi_tag("transparent")]);
    client_pool.publish_event(&m3).await.expect("publish");
    wait_until(
        "the transparent lifecycle must re-invoice the granted identity",
        Duration::from_secs(2),
        async || {
            server_events_containing(&pool, server_pubkey, "payment_required")
                .await
                .iter()
                .any(|e| all_tags(e).contains(&vec!["e".to_string(), m3.id.to_hex()]))
        },
    )
    .await;
    // This transparent payment settles and forwards on its own; drain it.
    let incoming = tokio::time::timeout(Duration::from_secs(3), server_rx.recv())
        .await
        .expect("the second transparent payment must forward after settling")
        .expect("channel open");
    assert_eq!(incoming.event_id, m3.id.to_hex());
    server
        .send_response(&incoming.event_id, result_response("mig-3"))
        .await
        .expect("respond");

    // 4) Update to explicit gating again: the grant minted in step 2 must still be
    // intact, so the retry claims and forwards with no new offer.
    let m4 = signed_paid_call(
        &keys,
        server_pubkey,
        "mig-4",
        vec![pi_tag("explicit_gating")],
    );
    client_pool.publish_event(&m4).await.expect("publish");
    let incoming = tokio::time::timeout(Duration::from_secs(3), server_rx.recv())
        .await
        .expect("the gating retry must claim the intact grant")
        .expect("channel open");
    assert_eq!(incoming.event_id, m4.id.to_hex());
    assert!(
        !server_events_containing(&pool, server_pubkey, "Payment Required")
            .await
            .iter()
            .any(|e| all_tags(e).contains(&vec!["e".to_string(), m4.id.to_hex()])),
        "the claiming retry must not draw a fresh offer"
    );

    server.close().await.expect("close");
}

// ── the announcement surface ────────────────────────────────────────────────

/// The kind 11316 announcement carries the composed payment surface in order: the
/// `pmi` tags in registration order, the availability tag last in the extra
/// segment (present only under the permissive policy), and the `cap` pricing tags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announcement_carries_the_payment_surface() {
    for (policy, expect_availability) in [
        (PaymentInteractionPolicy::Optional, true),
        (PaymentInteractionPolicy::Transparent, false),
    ] {
        let pool = Arc::new(MockRelayPool::new());
        let mut server = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default()
                .with_server_info(ServerInfo::default().with_name("announcer")),
            as_pool(&pool),
        )
        .await
        .expect("server transport");

        let options = ServerPaymentsOptions::new(
            vec![fake_processor("pmi:A", 0), fake_processor("pmi:B", 0)],
            vec![
                PricedCapability {
                    method: "tools/call".to_string(),
                    name: Some("add".to_string()),
                    amount: 1,
                    max_amount: None,
                    currency_unit: "sats".to_string(),
                    description: None,
                },
                PricedCapability {
                    method: "prompts/get".to_string(),
                    name: Some("summarize".to_string()),
                    amount: 5,
                    max_amount: Some(20),
                    currency_unit: "sats".to_string(),
                    description: None,
                },
            ],
        )
        .with_payment_interaction(policy);
        with_server_payments(&mut server, options).expect("register payments");

        server.announce().await.expect("announce");
        let announcement = pool
            .stored_events()
            .await
            .into_iter()
            .find(|e| e.kind == Kind::Custom(11316))
            .expect("announcement");

        // The payment tags, in composition order, filtered out of the surrounding
        // server-info and capability tags.
        let payment_tags: Vec<Vec<String>> = all_tags(&announcement)
            .into_iter()
            .filter(|t| {
                matches!(
                    t.first().map(String::as_str),
                    Some("pmi") | Some("payment_interaction") | Some("cap")
                )
            })
            .collect();
        let mut expected = vec![
            vec!["pmi".to_string(), "pmi:A".to_string()],
            vec!["pmi".to_string(), "pmi:B".to_string()],
        ];
        if expect_availability {
            expected.push(vec![
                "payment_interaction".to_string(),
                "explicit_gating".to_string(),
            ]);
        }
        expected.push(vec![
            "cap".to_string(),
            "tool:add".to_string(),
            "1".to_string(),
            "sats".to_string(),
        ]);
        expected.push(vec![
            "cap".to_string(),
            "prompt:summarize".to_string(),
            "5-20".to_string(),
            "sats".to_string(),
        ]);
        assert_eq!(
            payment_tags, expected,
            "policy {policy:?}: the announcement's payment surface must match"
        );
    }
}

// ── double registration refused ─────────────────────────────────────────────

/// A second registration on the same transport is refused, and a priced call is
/// charged exactly once: the double-charge a silently appended second middleware
/// pair would produce is structurally closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_registration_is_refused() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    with_server_payments(&mut server, payments_options(10_000))
        .expect("the first registration succeeds");
    let error = with_server_payments(&mut server, payments_options(10_000))
        .expect_err("a second registration must be refused");
    assert!(
        error
            .to_string()
            .contains("a payment interaction policy is already recorded"),
        "unexpected error: {error}"
    );

    let mut server_rx = server.take_message_receiver().expect("rx");
    server.start().await.expect("start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_timeout(Duration::from_secs(30)),
        Arc::new(client_pool),
    )
    .await
    .expect("client transport");
    client.start().await.expect("client start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // The verify is parked, so within this window every charge stays visible as its
    // own payment_required.
    client.send(&paid_call("once-1")).await.expect("send");
    wait_for_server_event(
        &pool,
        server_pubkey,
        "payment_required",
        Duration::from_secs(2),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let required = server_events_containing(&pool, server_pubkey, "payment_required").await;
    assert_eq!(
        required.len(),
        1,
        "one registered lifecycle charges one priced request exactly once"
    );
    assert!(server_rx.try_recv().is_err());

    server.close().await.expect("close");
}

// ── advertisement and disclosure dedup ──────────────────────────────────────

/// The first response of a gating session carries exactly one `payment_interaction`
/// tag: the replayed availability advertisement and the effective-mode disclosure
/// deduplicate through the production wiring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertisement_and_disclosure_dedup_on_first_response() {
    let mut fx = fixture(
        payments_options(10_000), // parked: this test is about the offer's tags
        |c| c,
        Some(PaymentInteractionMode::ExplicitGating),
    )
    .await;

    fx.client.send(&paid_call("dedup-1")).await.expect("send");
    let offer = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;
    let mode_tags: Vec<Vec<String>> = all_tags(&offer)
        .into_iter()
        .filter(|t| t.first().map(String::as_str) == Some("payment_interaction"))
        .collect();
    assert_eq!(
        mode_tags,
        vec![vec![
            "payment_interaction".to_string(),
            "explicit_gating".to_string()
        ]],
        "the advertisement and the disclosure must collapse to one tag"
    );

    fx.server.close().await.expect("close");
}
