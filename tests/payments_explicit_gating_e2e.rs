//! CEP-8 explicit-gating payment lifecycle, end to end over `MockRelayPool`.
//!
//! A real server transport with the explicit-gating middleware registered before
//! `start()`, a real client (a client transport or raw signed events, per test), and
//! every emission asserted on the wire: published events and their tags, never a
//! sender spy. The authorization store is injected, so tests observe lifecycle state
//! (pending, granted) directly.
//!
//! Clock discipline: the authorization store, the stale-route sweep and session expiry
//! all run on `std::time::Instant`, which paused tokio time does not advance, so every
//! test here runs on the real clock with tiny configured timeouts. A test that mixes
//! the paused tokio clock with a store-TTL assertion proves nothing while green, which
//! is why none does.

use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::core::constants::CTXVM_MESSAGES_KIND;
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::payments::compute_canonical_invocation_identity;
use contextvm_sdk::payments::fakes::{FakePaymentProcessor, FakePaymentProcessorOptions};
use contextvm_sdk::payments::types::PricedCapability;
use contextvm_sdk::payments::{
    create_explicit_gating_middleware, create_server_payments_middleware, AuthorizationStore,
    CanonicalInvocationIdentity, ExplicitGatingMiddlewareParams, PaymentInteractionPolicy,
    ServerPaymentsMiddlewareParams, ServerPaymentsOptions,
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

/// The canonical identity a `paid_call` resolves to for `client` (the id plays no part).
fn paid_call_identity(client: &PublicKey) -> CanonicalInvocationIdentity {
    compute_canonical_invocation_identity(
        &client.to_hex(),
        "tools/call",
        Some(&serde_json::json!({ "name": "paid-tool" })),
    )
    .expect("test params canonicalize")
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

/// Register the explicit-gating middleware on `server` with one fake processor and the
/// injected `store`, and let the server accept the `explicit_gating` negotiation.
fn register_gating(
    server: &mut NostrServerTransport,
    store: &AuthorizationStore,
    verify_delay_ms: u64,
) {
    let sender = server.targeted_response_sender();
    let options = ServerPaymentsOptions::new(
        vec![Arc::new(FakePaymentProcessor::with_options(
            FakePaymentProcessorOptions {
                pmi: "fake".to_string(),
                verify_delay_ms,
                create_delay_ms: 0,
                ttl: None,
            },
        ))],
        vec![priced_tool(21)],
    );
    server.add_inbound_middleware(create_explicit_gating_middleware(
        ExplicitGatingMiddlewareParams::new(options, sender, store.clone()),
    ));
    server.set_supported_payment_interaction(PaymentInteractionPolicy::Optional);
}

struct Fx {
    server: NostrServerTransport,
    server_rx: tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
    client: NostrClientTransport,
    client_rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    pool: Arc<MockRelayPool>,
    store: AuthorizationStore,
    client_pubkey: PublicKey,
    server_pubkey: PublicKey,
}

/// A paired client/server: the gating middleware registered before `start()`, the
/// client negotiating `explicit_gating` and advertising the fake PMI on its first
/// message.
async fn fixture(
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
    let store = AuthorizationStore::new();
    register_gating(&mut server, &store, verify_delay_ms);

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(client_encryption)
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_pmis(vec!["fake".to_string()])
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
        store,
        client_pubkey,
        server_pubkey,
    }
}

/// The `n`th (0-based, by stored order) client request event containing `id`.
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

// ── the stateless first exchange ────────────────────────────────────────────

/// The first priced invocation of a stateless gating client, pinned on the wire: the
/// `-32042` error event carries recipient, correlation, the one-shot discovery replay
/// and the effective-mode disclosure; the payload keeps the client's own inner request
/// id (never the event id); and the gated drop releases the route and wrap-kind
/// entries the transport reserved (the targeted publish itself consumes neither).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stateless_first_exchange_pins_the_wire() {
    let mut fx = fixture(
        10_000, // parked: this test is about the offer, not the settlement
        |c| c,
        EncryptionMode::Disabled,
        EncryptionMode::Disabled,
    )
    .await;

    fx.client.send(&paid_call("gate-1")).await.expect("send");
    let request_event = client_request_events(&fx.pool, fx.client_pubkey, "gate-1")
        .await
        .into_iter()
        .next()
        .expect("request stored");
    let request_event_id = request_event.id.to_hex();

    let offer = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;
    // The FULL tag list: recipient, correlation, the discovery replay (no server info
    // configured, so exactly the oversized-support capability), and the negotiated-mode
    // disclosure. Both one-shot latches ride this first targeted response.
    assert_eq!(
        all_tags(&offer),
        vec![
            vec!["p".to_string(), fx.client_pubkey.to_hex()],
            vec!["e".to_string(), request_event_id.clone()],
            vec!["support_oversized_transfer".to_string()],
            vec![
                "payment_interaction".to_string(),
                "explicit_gating".to_string()
            ],
        ],
        "the offer must compose the same tags as any other first response"
    );
    // The payload keeps the client's own request id and offers exactly one option.
    assert!(
        offer.content.contains("\"id\":\"gate-1\""),
        "the error id must be the original inner request id, got {}",
        offer.content
    );
    assert!(offer.content.contains("\"code\":-32042"));
    assert!(offer.content.contains("\"pmi\":\"fake\""));

    // The targeted publish itself never consumes the correlation route (pinned at the
    // sender's own test suite); what ends the route here is the seam's drop-cleanup
    // for the gated request, which must release BOTH the route and the wrap-kind
    // entry the transport reserved before dispatch.
    wait_until(
        "the gated drop released its route and wrap-kind entry",
        Duration::from_secs(2),
        async || {
            !fx.server.has_event_route(&request_event_id).await
                && !fx.server.has_request_wrap_kind(&request_event_id).await
        },
    )
    .await;
    // And the gated request never reached the handler.
    assert!(fx.server_rx.try_recv().is_err());

    fx.server.close().await.expect("close");
}

// ── grant and claim ─────────────────────────────────────────────────────────

/// Pay, verify, retry: the fake settles, the verify grants, and a retry under a NEW
/// event id claims the grant, forwards, and the worker's response delivers on the
/// retry's own route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_then_retry_delivers_the_result() {
    let mut fx = fixture(
        50,
        |c| c,
        EncryptionMode::Disabled,
        EncryptionMode::Disabled,
    )
    .await;
    let identity = paid_call_identity(&fx.client_pubkey);

    fx.client.send(&paid_call("pay-1")).await.expect("send");
    wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;
    let first_event_id = client_request_events(&fx.pool, fx.client_pubkey, "pay-1").await[0]
        .id
        .to_hex();

    // The fake settles after 50 ms; the grant pops the pending entry.
    wait_until("the verify granted", Duration::from_secs(2), async || {
        fx.store.get_pending_remaining_ms(&identity) == 0
    })
    .await;

    // The retry is a FRESH request event with the same method and params.
    fx.client.send(&paid_call("pay-1")).await.expect("retry");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid retry must reach the handler")
        .expect("channel open");
    assert_ne!(
        incoming.event_id, first_event_id,
        "the claiming invocation is a new event"
    );

    fx.server
        .send_response(&incoming.event_id, result_response("pay-1"))
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
    assert!(response.content.contains("\"pay-1\""));

    // Single use: nothing further is claimable.
    assert!(!fx.store.claim(&identity));

    fx.server.close().await.expect("close");
}

/// A duplicate invocation during the verification window draws `-32043` on the wire,
/// correlated to the DUPLICATE's own event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_window_returns_32043_on_the_wire() {
    let mut fx = fixture(
        10_000, // parked, so the pending window stays open
        |c| c,
        EncryptionMode::Disabled,
        EncryptionMode::Disabled,
    )
    .await;

    fx.client.send(&paid_call("pend-1")).await.expect("send");
    wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;

    fx.client.send(&paid_call("pend-1")).await.expect("dup");
    let pending = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Payment Pending",
        Duration::from_secs(2),
    )
    .await;
    assert!(pending.content.contains("\"code\":-32043"));
    assert!(pending.content.contains("\"retry_after\":"));

    // Correlated to the duplicate's event, not the original's.
    let requests = client_request_events(&fx.pool, fx.client_pubkey, "pend-1").await;
    assert_eq!(requests.len(), 2);
    let duplicate_id = requests[1].id.to_hex();
    assert!(
        all_tags(&pending).contains(&vec!["e".to_string(), duplicate_id]),
        "-32043 must answer the duplicate's own event"
    );
    assert!(fx.server_rx.try_recv().is_err());

    fx.server.close().await.expect("close");
}

// ── encryption ──────────────────────────────────────────────────────────────

/// The whole gating cycle under required encryption: the offer error reaches the
/// client through a gift wrap (decrypted and correlated client-side), the retry
/// claims, and nothing the session produced is plaintext ContextVM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_end_to_end() {
    let mut fx = fixture(
        50,
        |c| c,
        EncryptionMode::Required,
        EncryptionMode::Required,
    )
    .await;
    let identity = paid_call_identity(&fx.client_pubkey);

    fx.client.send(&paid_call("enc-1")).await.expect("send");

    // The client decrypts the offer off its gift wrap: wrap-kind mirroring and the
    // inner-tag correlation work for a targeted error.
    let offer = tokio::time::timeout(Duration::from_secs(3), fx.client_rx.recv())
        .await
        .expect("the client must decrypt and receive the offer")
        .expect("client channel open");
    match offer {
        JsonRpcMessage::ErrorResponse(response) => {
            assert_eq!(response.error.code, -32042);
            assert_eq!(response.id, serde_json::json!("enc-1"));
            let data = response.error.data.expect("offer data");
            assert_eq!(data["payment_options"][0]["pmi"], serde_json::json!("fake"));
        }
        other => panic!("expected the -32042 offer, got {other:?}"),
    }

    wait_until("the verify granted", Duration::from_secs(2), async || {
        fx.store.get_pending_remaining_ms(&identity) == 0
    })
    .await;

    fx.client.send(&paid_call("enc-1")).await.expect("retry");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid retry must reach the handler")
        .expect("channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("enc-1"))
        .await
        .expect("respond");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Nothing plaintext; every emission rides a gift wrap (request, offer, retry,
    // result at minimum).
    let events = fx.pool.stored_events().await;
    assert!(
        !events
            .iter()
            .any(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)),
        "an encrypted gating session must never emit plaintext ContextVM traffic"
    );
    let wraps = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(1059) || e.kind == Kind::Custom(21059))
        .count();
    assert!(
        wraps >= 4,
        "the request, the offer, the retry and the result must each ride a gift wrap; \
         saw {wraps}"
    );

    fx.server.close().await.expect("close");
}

// ── the oversized re-inject dispatch site ───────────────────────────────────

/// A CEP-22 reassembled priced call is gated on the re-inject dispatch site: the offer
/// correlates to the end frame's event, the dropped request's wrap-kind entry is
/// released (the re-inject path has no other reclaimer), and a paid oversized retry
/// claims and round-trips. The re-injected context presents no client PMIs (they rode
/// the start frame), so the offer takes the first configured processor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_gated_request_round_trip() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    let store = AuthorizationStore::new();
    register_gating(&mut server, &store, 50);
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

    // An oversized priced request as the FIRST send (a small send first would spend
    // the discovery tags and suppress the server's accept), with a progressToken
    // (nothing fragments without one).
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

    // The reassembled request is gated: the offer correlates to the end frame's event.
    let offer = wait_for_server_event(
        &pool,
        server_pubkey,
        "Payment Required",
        Duration::from_secs(5),
    )
    .await;
    let end_frame_event_id = all_tags(&offer)
        .into_iter()
        .find(|t| t[0] == "e")
        .expect("correlation tag")[1]
        .clone();
    assert!(
        offer.content.contains("\"pmi\":\"fake\""),
        "with no PMIs on the re-injected context the offer takes processors[0]"
    );

    // The dropped request's wrap-kind entry is released by the seam's cleanup: on this
    // path nothing else (no responder, no sweep, no session cleanup) can ever reclaim
    // it.
    wait_until(
        "the oversized drop released its wrap-kind entry",
        Duration::from_secs(2),
        async || !server.has_request_wrap_kind(&end_frame_event_id).await,
    )
    .await;

    // The fake settles; the oversized retry claims, forwards, and round-trips.
    let identity = compute_canonical_invocation_identity(
        &client_pubkey.to_hex(),
        "tools/call",
        Some(&serde_json::json!({
            "name": "paid-tool",
            "arguments": { "blob": blob },
        })),
    )
    .expect("canonicalize");
    wait_until("the verify granted", Duration::from_secs(2), async || {
        store.get_pending_remaining_ms(&identity) == 0
    })
    .await;

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

// ── shutdown ────────────────────────────────────────────────────────────────

/// `close()` cancels a parked verification through the token cascade: the fake
/// observes the cancellation, the task clears the pending state, and no grant is
/// minted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_a_parked_verify() {
    let mut fx = fixture(
        30_000,
        |c| c,
        EncryptionMode::Disabled,
        EncryptionMode::Disabled,
    )
    .await;
    let identity = paid_call_identity(&fx.client_pubkey);

    fx.client.send(&paid_call("shut-1")).await.expect("send");
    wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;
    assert!(fx.store.get_pending_remaining_ms(&identity) > 0);

    let close_started = tokio::time::Instant::now();
    fx.server.close().await.expect("close");
    assert!(
        close_started.elapsed() < Duration::from_secs(5),
        "close must not wait out a 30 s verify"
    );

    // The cancelled verify fails: pending is cleared and nothing was granted.
    wait_until(
        "the cancelled verify cleared its pending state",
        Duration::from_secs(2),
        async || fx.store.get_pending_remaining_ms(&identity) == 0,
    )
    .await;
    assert!(
        !fx.store.claim(&identity),
        "a cancelled verification must never grant"
    );
}

// ── both middlewares registered ─────────────────────────────────────────────

/// The production chain shape (transparent first, gating second, both self-gated): a
/// gating session's full offer-verify-grant-claim cycle succeeds through the
/// transparent middleware's pass-through, and a transparent session's priced request
/// never touches the authorization store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_middlewares_registered_partition_by_mode() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let pool = Arc::new(server_pool);
    let raw_client_pool = Arc::new(client_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(&pool),
    )
    .await
    .expect("server transport");

    // Transparent first (the ts registration order), gating second.
    let notification_sender = server.payment_notification_sender(Duration::from_secs(300));
    server.add_inbound_middleware(create_server_payments_middleware(
        ServerPaymentsMiddlewareParams::new(
            ServerPaymentsOptions::new(
                vec![Arc::new(FakePaymentProcessor::with_options(
                    FakePaymentProcessorOptions {
                        pmi: "fake".to_string(),
                        verify_delay_ms: 50,
                        create_delay_ms: 0,
                        ttl: None,
                    },
                ))],
                vec![priced_tool(21)],
            ),
            notification_sender,
        ),
    ));
    let store = AuthorizationStore::new();
    register_gating(&mut server, &store, 50);

    let mut server_rx = server.take_message_receiver().expect("rx");
    server.start().await.expect("start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Client A negotiates explicit gating.
    let mut gating_client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
            .with_pmis(vec!["fake".to_string()])
            .with_timeout(Duration::from_secs(30)),
        Arc::clone(&raw_client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("gating client");
    let gating_pubkey = raw_client_pool.mock_public_key();
    gating_client.start().await.expect("start");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A's full gating cycle: offer, settle, retry, claim, forward.
    gating_client.send(&paid_call("mix-a")).await.expect("send");
    wait_for_server_event(
        &pool,
        server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;
    let gating_identity = paid_call_identity(&gating_pubkey);
    wait_until("A's verify granted", Duration::from_secs(2), async || {
        store.get_pending_remaining_ms(&gating_identity) == 0
    })
    .await;
    gating_client
        .send(&paid_call("mix-a"))
        .await
        .expect("retry");
    let incoming_a = tokio::time::timeout(Duration::from_secs(3), server_rx.recv())
        .await
        .expect("A's paid retry must reach the handler")
        .expect("channel open");

    // Client B: raw events with NO payment_interaction tag, so the session resolves to
    // transparent and B's priced request runs the transparent lifecycle.
    let b_keys = Keys::generate();
    let b_call = contextvm_sdk::core::serializers::mcp_to_nostr_event(
        &paid_call("mix-b"),
        CTXVM_MESSAGES_KIND,
        BaseTransport::create_recipient_tags(&server_pubkey),
    )
    .expect("serialize")
    .sign_with_keys(&b_keys)
    .expect("sign");
    raw_client_pool
        .publish_event(&b_call)
        .await
        .expect("publish");

    // B's request runs the transparent lifecycle: a payment_required NOTIFICATION goes
    // out and, after settlement, the request forwards.
    wait_for_server_event(
        &pool,
        server_pubkey,
        "notifications/payment_required",
        Duration::from_secs(2),
    )
    .await;
    let incoming_b = tokio::time::timeout(Duration::from_secs(3), server_rx.recv())
        .await
        .expect("B's paid request must reach the handler")
        .expect("channel open");
    assert_ne!(incoming_a.event_id, incoming_b.event_id);

    // Cross-lifecycle no-migration: B's cycle never touched the authorization store.
    let b_identity = paid_call_identity(&b_keys.public_key());
    assert_eq!(
        store.get_pending_remaining_ms(&b_identity),
        0,
        "a transparent session must never write gating pending state"
    );
    assert!(
        !store.claim(&b_identity),
        "a transparent session must never mint a gating grant"
    );

    server.close().await.expect("close");
}

// ── route lifetime independence ─────────────────────────────────────────────

/// Gating state outlives the request's route on purpose: the offer's route is popped
/// at the gated drop, the sweep interval passes, and the retry still claims and gets
/// its result, because the claiming invocation is a FRESH request with its own route.
/// This is why the gating lifecycle needs none of the transparent lifecycle's route
/// snapshot machinery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gating_state_survives_route_sweep() {
    let mut fx = fixture(
        100,
        |c| {
            c.with_request_timeout(Duration::from_millis(100))
                .with_cleanup_interval(Duration::from_millis(50))
        },
        EncryptionMode::Disabled,
        EncryptionMode::Disabled,
    )
    .await;
    let identity = paid_call_identity(&fx.client_pubkey);

    fx.client.send(&paid_call("sweep-1")).await.expect("send");
    wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "Payment Required",
        Duration::from_secs(2),
    )
    .await;

    wait_until("the verify granted", Duration::from_secs(2), async || {
        fx.store.get_pending_remaining_ms(&identity) == 0
    })
    .await;

    // Wait past several sweep ticks: every route from the first exchange is long gone.
    tokio::time::sleep(Duration::from_millis(300)).await;

    fx.client.send(&paid_call("sweep-1")).await.expect("retry");
    let incoming = tokio::time::timeout(Duration::from_secs(3), fx.server_rx.recv())
        .await
        .expect("the paid retry must reach the handler")
        .expect("channel open");
    fx.server
        .send_response(&incoming.event_id, result_response("sweep-1"))
        .await
        .expect("the retry's own fresh route delivers the result");
    let response = wait_for_server_event(
        &fx.pool,
        fx.server_pubkey,
        "\"content\"",
        Duration::from_secs(2),
    )
    .await;
    assert!(
        all_tags(&response).contains(&vec!["e".to_string(), incoming.event_id.clone()]),
        "the result correlates to the claiming invocation"
    );

    fx.server.close().await.expect("close");
}

// ── the transparent duplicate-drop blast radius ─────────────────────────────

/// The wrap-kind release's cross-middleware cell: a transparent session's duplicate
/// delivery joins the in-flight payment and drops, its cleanup pops the original's
/// route AND releases the wrap-kind entry, and the original's paid response still
/// delivers from the snapshot (which carries its own wrap kind), unharmed by the
/// removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transparent_duplicate_drop_leaves_snapshot_delivery_intact() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_cleanup_interval(Duration::from_millis(50)),
        as_pool(&pool),
    )
    .await
    .expect("server transport");
    let notification_sender = server.payment_notification_sender(Duration::from_secs(300));
    server.add_inbound_middleware(create_server_payments_middleware(
        ServerPaymentsMiddlewareParams::new(
            ServerPaymentsOptions::new(
                vec![Arc::new(FakePaymentProcessor::with_options(
                    FakePaymentProcessorOptions {
                        pmi: "fake".to_string(),
                        verify_delay_ms: 400,
                        create_delay_ms: 0,
                        ttl: None,
                    },
                ))],
                vec![priced_tool(21)],
            ),
            notification_sender,
        ),
    ));
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

    client.send(&paid_call("dup-1")).await.expect("send");
    let request_event = client_request_events(&pool, client_pubkey, "dup-1")
        .await
        .into_iter()
        .next()
        .expect("request stored");
    let request_event_id = request_event.id.to_hex();

    // Redeliver the identical event while the payment is in flight; the duplicate's
    // chain run drops and its cleanup pops the route and the wrap-kind entry.
    tokio::time::sleep(Duration::from_millis(100)).await;
    pool.publish_event(&request_event).await.expect("redeliver");
    wait_until(
        "the duplicate's cleanup released route and wrap-kind entry",
        Duration::from_secs(2),
        async || {
            !server.has_event_route(&request_event_id).await
                && !server.has_request_wrap_kind(&request_event_id).await
        },
    )
    .await;

    // Exactly one delivery, one charge, and the response still delivers.
    let incoming = tokio::time::timeout(Duration::from_secs(3), server_rx.recv())
        .await
        .expect("the paid request must reach the handler")
        .expect("channel open");
    assert_eq!(incoming.event_id, request_event_id);
    let required =
        server_events_containing(&pool, server_pubkey, "notifications/payment_required").await;
    assert_eq!(required.len(), 1, "the duplicate must not re-invoice");

    server
        .send_response(&request_event_id, result_response("dup-1"))
        .await
        .expect("the snapshot path must deliver after the duplicate's cleanup");
    let response =
        wait_for_server_event(&pool, server_pubkey, "\"content\"", Duration::from_secs(2)).await;
    assert!(response.content.contains("\"dup-1\""));
    assert!(all_tags(&response).contains(&vec!["e".to_string(), request_event_id]));

    server.close().await.expect("close");
}
