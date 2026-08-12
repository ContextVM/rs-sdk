//! CEP-8 client-side payment-interaction negotiation, end to end over `MockRelayPool`.
//!
//! Every emission assertion reads the tag list off a *published event* rather than
//! calling the transport's internal getter, so the wiring into the send path and the
//! threading through its publish sites are covered by the same assertions. The
//! getter's own rules are unit-tested in `src/transport/client/mod.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use contextvm_sdk::core::constants::{tags, CTXVM_MESSAGES_KIND};
use contextvm_sdk::core::types::{EncryptionMode, PaymentInteractionMode};
use contextvm_sdk::payments::tags::payment_interaction_tag;
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::oversized_transfer::OversizedTransferConfig;
use contextvm_sdk::transport::server::{
    IncomingRequest, NostrServerTransport, NostrServerTransportConfig,
};
use contextvm_sdk::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, PaymentInteractionPolicy, RelayPoolTrait, ServerInfo,
};
use nostr_sdk::prelude::*;

/// Let spawned event loops call `notifications()` before we publish anything.
async fn let_event_loops_start() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

fn request(id: &str, method: &str) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: method.to_string(),
        params: None,
    })
}

fn notification(method: &str) -> JsonRpcMessage {
    JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: method.to_string(),
        params: None,
    })
}

/// Every tag on `event`, flattened so a whole tag list can be compared in one
/// assertion.
fn all_tags(event: &Event) -> Vec<Vec<String>> {
    event.tags.iter().map(|t| t.clone().to_vec()).collect()
}

fn tag(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn has_mode_tag(event: &Event) -> bool {
    all_tags(event)
        .iter()
        .any(|t| t.first().map(String::as_str) == Some(tags::PAYMENT_INTERACTION))
}

/// The client request carrying `id`, as stored in the shared mock relay network.
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

/// The client event whose content contains `needle` (for notifications, which have no
/// request id).
async fn client_event_containing(
    pool: &Arc<MockRelayPool>,
    client: PublicKey,
    needle: &str,
) -> Event {
    pool.stored_events()
        .await
        .into_iter()
        .find(|e| {
            e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)
                && e.pubkey == client
                && e.content.contains(needle)
        })
        .unwrap_or_else(|| panic!("client event containing {needle} missing from the relay store"))
}

/// A paired client/server over one mock relay network. `EncryptionMode::Disabled` and
/// otherwise default config, so the client's one-shot discovery set is exactly
/// `support_oversized_transfer`.
struct Fixture {
    client: NostrClientTransport,
    server: NostrServerTransport,
    server_rx: tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
    pool: Arc<MockRelayPool>,
    client_pubkey: PublicKey,
    server_pubkey: PublicKey,
}

async fn fixture(
    configure_client: impl FnOnce(NostrClientTransportConfig) -> NostrClientTransportConfig,
    configure_server: impl FnOnce(&mut NostrServerTransport),
) -> Fixture {
    fixture_with_encryption(EncryptionMode::Disabled, configure_client, configure_server).await
}

async fn fixture_with_encryption(
    encryption: EncryptionMode,
    configure_client: impl FnOnce(NostrClientTransportConfig) -> NostrClientTransportConfig,
    configure_server: impl FnOnce(&mut NostrServerTransport),
) -> Fixture {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(encryption),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");
    configure_server(&mut server);

    let client_config = configure_client(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(encryption),
    );
    let mut client = NostrClientTransport::with_relay_pool(client_config, Arc::new(client_pool))
        .await
        .expect("create client transport");

    let server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    Fixture {
        client,
        server,
        server_rx,
        pool: server_pool,
        client_pubkey,
        server_pubkey,
    }
}

/// Drive one request through to the server and answer it, so the response (and any
/// disclosure it carries) makes it back to the client.
async fn round_trip(fx: &mut Fixture, id: &str, method: &str) {
    fx.client.send(&request(id, method)).await.expect("send");
    let incoming = tokio::time::timeout(Duration::from_millis(1000), fx.server_rx.recv())
        .await
        .expect("request must reach the server")
        .expect("server channel closed");
    fx.server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(id),
                result: serde_json::json!({ "content": [] }),
            }),
        )
        .await
        .expect("send response");
    // Give the client's event loop a turn to learn from the response.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ── Emission and the latch ──────────────────────────────────────────────────

/// The full negotiation set rides request 1; request 2 keeps only the unlatched `pmi`
/// tags. Asserts the *entire* tag list of both, in composition order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negotiation_tags_ride_the_first_request_and_latch() {
    let fx = fixture(
        |c| {
            c.with_pmis(vec!["lightning".to_string(), "cashu".to_string()])
                .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
        },
        |_| {},
    )
    .await;
    let server_hex = fx.server_pubkey.to_hex();

    fx.client
        .send(&request("neg-1", "tools/list"))
        .await
        .unwrap();
    fx.client
        .send(&request("neg-2", "tools/list"))
        .await
        .unwrap();

    let first = client_request_event(&fx.pool, fx.client_pubkey, "neg-1").await;
    assert_eq!(
        all_tags(&first),
        vec![
            tag(&["p", &server_hex]),
            tag(&[tags::SUPPORT_OVERSIZED_TRANSFER]),
            tag(&["pmi", "lightning"]),
            tag(&["pmi", "cashu"]),
            tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"]),
        ],
        "request 1 must carry routing, one-shot discovery, the pmi tags in preference \
         order, and the requested mode"
    );

    let second = client_request_event(&fx.pool, fx.client_pubkey, "neg-2").await;
    assert_eq!(
        all_tags(&second),
        vec![
            tag(&["p", &server_hex]),
            tag(&["pmi", "lightning"]),
            tag(&["pmi", "cashu"]),
        ],
        "request 2 must keep the unlatched pmi tags but drop both the one-shot \
         discovery tags and the already-published payment_interaction"
    );
}

/// A notification sent *first* neither emits the tags nor burns the latch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notifications_carry_no_negotiation_tags_and_do_not_latch() {
    let fx = fixture(
        |c| {
            c.with_pmis(vec!["lightning".to_string()])
                .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
        },
        |_| {},
    )
    .await;
    let server_hex = fx.server_pubkey.to_hex();

    fx.client
        .send(&notification("notifications/initialized"))
        .await
        .unwrap();
    fx.client
        .send(&request("after-notif", "tools/list"))
        .await
        .unwrap();

    let notif =
        client_event_containing(&fx.pool, fx.client_pubkey, "notifications/initialized").await;
    assert_eq!(
        all_tags(&notif),
        vec![tag(&["p", &server_hex])],
        "a notification carries bare recipient tags: no discovery, no pmi, no mode"
    );

    let req = client_request_event(&fx.pool, fx.client_pubkey, "after-notif").await;
    assert_eq!(
        all_tags(&req),
        vec![
            tag(&["p", &server_hex]),
            tag(&[tags::SUPPORT_OVERSIZED_TRANSFER]),
            tag(&["pmi", "lightning"]),
            tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"]),
        ],
        "the leading notification must not have burnt either latch"
    );
}

/// A notification *between* two requests disturbs neither an armed nor a burnt latch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_notification_between_requests_neither_emits_nor_latches() {
    let fx = fixture(
        |c| c.with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        |_| {},
    )
    .await;
    let server_hex = fx.server_pubkey.to_hex();

    fx.client
        .send(&request("mid-1", "tools/list"))
        .await
        .unwrap();
    fx.client
        .send(&notification("notifications/cancelled"))
        .await
        .unwrap();
    fx.client
        .send(&request("mid-2", "tools/list"))
        .await
        .unwrap();

    let first = client_request_event(&fx.pool, fx.client_pubkey, "mid-1").await;
    assert!(
        all_tags(&first).contains(&tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"])),
        "request 1 establishes the mode"
    );

    let notif =
        client_event_containing(&fx.pool, fx.client_pubkey, "notifications/cancelled").await;
    assert_eq!(
        all_tags(&notif),
        vec![tag(&["p", &server_hex])],
        "the interleaved notification emits nothing"
    );

    let second = client_request_event(&fx.pool, fx.client_pubkey, "mid-2").await;
    assert_eq!(
        all_tags(&second),
        vec![tag(&["p", &server_hex])],
        "the latch stays burnt across the notification: no re-send"
    );
}

/// PMIs configured, no requested mode: every request carries the `pmi` tags and never
/// a `payment_interaction` tag. Also covers an empty list emitting nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pmi_only_client_emits_pmi_and_no_mode_tag() {
    let fx = fixture(|c| c.with_pmis(vec!["lightning".to_string()]), |_| {}).await;

    fx.client
        .send(&request("pmi-1", "tools/list"))
        .await
        .unwrap();
    fx.client
        .send(&request("pmi-2", "tools/list"))
        .await
        .unwrap();

    for id in ["pmi-1", "pmi-2"] {
        let event = client_request_event(&fx.pool, fx.client_pubkey, id).await;
        assert!(
            all_tags(&event).contains(&tag(&["pmi", "lightning"])),
            "{id}: pmi tags are never latched"
        );
        assert!(
            !has_mode_tag(&event),
            "{id}: no mode was requested, so no payment_interaction tag may be emitted"
        );
    }

    // An explicitly empty list emits nothing at all.
    let empty = fixture(|c| c.with_pmis(vec![]), |_| {}).await;
    empty
        .client
        .send(&request("pmi-empty", "tools/list"))
        .await
        .unwrap();
    let event = client_request_event(&empty.pool, empty.client_pubkey, "pmi-empty").await;
    assert_eq!(
        all_tags(&event),
        vec![
            tag(&["p", &empty.server_pubkey.to_hex()]),
            tag(&[tags::SUPPORT_OVERSIZED_TRANSFER]),
        ],
        "with_pmis(vec![]) must emit no pmi tags"
    );
}

/// The live-path no-op: a client with no payments configuration publishes exactly the
/// tag list it published before CEP-8 client negotiation existed. This is a claim about
/// the wire, so it asserts the entire published tag list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconfigured_client_emits_the_same_tags_as_before() {
    let fx = fixture(|c| c, |_| {}).await;
    let server_hex = fx.server_pubkey.to_hex();

    fx.client
        .send(&request("plain-1", "tools/list"))
        .await
        .unwrap();
    fx.client
        .send(&request("plain-2", "tools/list"))
        .await
        .unwrap();

    let first = client_request_event(&fx.pool, fx.client_pubkey, "plain-1").await;
    assert_eq!(
        all_tags(&first),
        vec![
            tag(&["p", &server_hex]),
            tag(&[tags::SUPPORT_OVERSIZED_TRANSFER]),
        ],
        "an unconfigured client's first request is routing plus one-shot discovery, \
         exactly as before"
    );

    let second = client_request_event(&fx.pool, fx.client_pubkey, "plain-2").await;
    assert_eq!(
        all_tags(&second),
        vec![tag(&["p", &server_hex])],
        "and its second request is bare recipient tags, exactly as before"
    );
}

/// The second latch site: an oversized first request latches on its `start` frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_first_request_latches_the_negotiation_tag() {
    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(6000)
        .with_chunk_size(6000);
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized)
            .with_pmis(vec!["lightning".to_string()])
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        Arc::new(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("big-1"),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "_meta": { "progressToken": "tok-neg" },
                "blob": "A".repeat(10000),
            })),
        }))
        .await
        .expect("send oversized request");

    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("oversized request must reassemble")
        .expect("server channel closed");
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("big-1")));

    let frame_type = |e: &Event| {
        serde_json::from_str::<serde_json::Value>(&e.content)
            .ok()
            .as_ref()
            .and_then(|v| v.get("params"))
            .and_then(|p| p.get("cvm"))
            .and_then(|c| c.get("frameType"))
            .and_then(|f| f.as_str())
            .map(str::to_string)
    };
    let events = server_pool.stored_events().await;
    let client_frames: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND) && e.pubkey == client_pubkey)
        .collect();
    let start = client_frames
        .iter()
        .find(|e| frame_type(e).as_deref() == Some("start"))
        .expect("start frame missing");
    let continuations: Vec<&&Event> = client_frames
        .iter()
        .filter(|e| matches!(frame_type(e).as_deref(), Some("chunk") | Some("end")))
        .collect();

    assert_eq!(
        all_tags(start),
        vec![
            tag(&["p", &server_pubkey.to_hex()]),
            tag(&[tags::SUPPORT_OVERSIZED_TRANSFER]),
            tag(&["pmi", "lightning"]),
            tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"]),
        ],
        "the negotiation set rides the start frame"
    );
    assert!(!continuations.is_empty(), "expected chunk/end frames");
    for frame in &continuations {
        assert_eq!(
            all_tags(frame),
            vec![tag(&["p", &server_pubkey.to_hex()])],
            "continuation frames carry bare recipient tags"
        );
    }

    // The whole point of the second latch site: a following normal request must not
    // re-send the mode.
    client
        .send(&request("after-big", "tools/list"))
        .await
        .unwrap();
    let follow_up = client_request_event(&server_pool, client_pubkey, "after-big").await;
    assert_eq!(
        all_tags(&follow_up),
        vec![
            tag(&["p", &server_pubkey.to_hex()]),
            tag(&["pmi", "lightning"]),
        ],
        "an oversized first request must burn the latch just as a normal one does"
    );
}

// ── Mid-session change ──────────────────────────────────────────────────────

/// A mid-session mode change re-emits the tag exactly once, and reaches the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_session_mode_change_re_emits_the_tag() {
    let mut fx = fixture(
        |c| c.with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        |s| s.set_supported_payment_interaction(PaymentInteractionPolicy::Optional),
    )
    .await;
    let server_hex = fx.server_pubkey.to_hex();

    round_trip(&mut fx, "chg-1", "tools/list").await;
    let first = client_request_event(&fx.pool, fx.client_pubkey, "chg-1").await;
    assert!(
        all_tags(&first).contains(&tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"])),
        "request 1 establishes explicit_gating"
    );
    assert_eq!(
        fx.client.get_effective_payment_interaction(),
        Some(PaymentInteractionMode::ExplicitGating),
        "the server's disclosure must have been recorded before the downgrade"
    );

    fx.client
        .set_payment_interaction(PaymentInteractionMode::Transparent);
    round_trip(&mut fx, "chg-2", "tools/list").await;
    let second = client_request_event(&fx.pool, fx.client_pubkey, "chg-2").await;
    assert!(
        all_tags(&second).contains(&tag(&[tags::PAYMENT_INTERACTION, "transparent"])),
        "the changed mode must reach the server, and transparent is emitted explicitly"
    );

    fx.client
        .send(&request("chg-3", "tools/list"))
        .await
        .unwrap();
    let third = client_request_event(&fx.pool, fx.client_pubkey, "chg-3").await;
    assert_eq!(
        all_tags(&third),
        vec![tag(&["p", &server_hex])],
        "the changed mode is latched in turn: request 3 carries no tag"
    );

    let snap = fx
        .server
        .session_snapshot(&fx.client_pubkey.to_hex())
        .await
        .expect("server session");
    assert_eq!(
        snap.requested_payment_interaction,
        Some(PaymentInteractionMode::Transparent),
        "the server's session must follow the client's mid-session change"
    );

    // A client that downgrades itself freezes its observed effective mode: the learner
    // gate is keyed on *requesting* explicit_gating, so the server's later disclosure is
    // no longer recorded. This matches the TypeScript SDK, and is pinned here
    // deliberately so a reader does not mistake it for a bug.
    assert_eq!(
        fx.client.get_effective_payment_interaction(),
        Some(PaymentInteractionMode::ExplicitGating),
        "the observed mode keeps its last value after a self-initiated downgrade"
    );
}

/// Re-requesting the mode already published emits nothing: the latch is keyed on what
/// went on the wire, not on the setter having been called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_requesting_the_same_mode_does_not_re_emit() {
    let fx = fixture(
        |c| c.with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        |_| {},
    )
    .await;
    let server_hex = fx.server_pubkey.to_hex();

    fx.client
        .send(&request("same-1", "tools/list"))
        .await
        .unwrap();
    fx.client
        .set_payment_interaction(PaymentInteractionMode::ExplicitGating);
    fx.client
        .send(&request("same-2", "tools/list"))
        .await
        .unwrap();

    let second = client_request_event(&fx.pool, fx.client_pubkey, "same-2").await;
    assert_eq!(
        all_tags(&second),
        vec![tag(&["p", &server_hex])],
        "re-setting the mode already published must not re-emit the tag"
    );
}

// ── The negotiation reaches the server ──────────────────────────────────────

/// Both halves of the handshake in one process: what this client emits is what the
/// server negotiates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_negotiates_the_mode_the_client_emitted() {
    let mut fx = fixture(
        |c| c.with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        |s| s.set_supported_payment_interaction(PaymentInteractionPolicy::Optional),
    )
    .await;

    fx.client
        .send(&request("e2e-1", "tools/list"))
        .await
        .unwrap();
    let incoming = tokio::time::timeout(Duration::from_millis(1000), fx.server_rx.recv())
        .await
        .expect("request must reach the server")
        .expect("server channel closed");

    let snap = fx
        .server
        .session_snapshot(&incoming.client_pubkey)
        .await
        .expect("server must have a session for the client");
    assert_eq!(
        snap.requested_payment_interaction,
        Some(PaymentInteractionMode::ExplicitGating),
        "the server must read the mode this client requested"
    );
    assert_eq!(
        snap.effective_payment_interaction,
        Some(PaymentInteractionMode::ExplicitGating),
        "an Optional-policy server grants the requested mode"
    );
}

// ── Inbound learning ────────────────────────────────────────────────────────

/// The client records the effective mode the server discloses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_records_the_disclosed_effective_mode() {
    let mut fx = fixture(
        |c| c.with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        |s| s.set_supported_payment_interaction(PaymentInteractionPolicy::Optional),
    )
    .await;

    assert_eq!(fx.client.get_effective_payment_interaction(), None);
    round_trip(&mut fx, "learn-1", "tools/list").await;
    assert_eq!(
        fx.client.get_effective_payment_interaction(),
        Some(PaymentInteractionMode::ExplicitGating),
        "the client must record the mode the server disclosed"
    );
}

/// A client that requested nothing treats an inbound `payment_interaction` tag as a
/// server availability advertisement and ignores it, while still learning the event's
/// other discovery tags. That second half is what distinguishes "ignored the tag" from
/// "dropped the event".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_ignores_an_advertisement_when_it_did_not_request_gating() {
    let mut fx = fixture(
        |c| c,
        |s| {
            s.set_supported_payment_interaction(PaymentInteractionPolicy::Optional);
            s.set_announcement_extra_tags(vec![payment_interaction_tag(
                PaymentInteractionMode::ExplicitGating,
            )]);
        },
    )
    .await;

    round_trip(&mut fx, "advert-1", "tools/list").await;

    assert_eq!(
        fx.client.get_effective_payment_interaction(),
        None,
        "an advertisement must not be recorded as this session's effective mode"
    );
    assert!(
        fx.client.get_server_initialize_event().is_some(),
        "the same event must otherwise have been learned, or this test proves nothing"
    );
}

/// The client records a *disclosed downgrade*, which means it records the observed
/// value rather than the one it requested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_records_a_disclosed_downgrade() {
    let mut fx = fixture(
        |c| c.with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        |s| s.set_supported_payment_interaction(PaymentInteractionPolicy::Transparent),
    )
    .await;

    // Request 1 draws the -32602 rejection, which carries bare routing tags and so
    // teaches the client nothing.
    fx.client
        .send(&request("down-1", "tools/list"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        fx.client.get_effective_payment_interaction(),
        None,
        "the rejection carries no payment_interaction tag, so nothing is learned from it"
    );

    // Request 2's response carries the disclosure. Two preconditions this test depends
    // on, asserted rather than assumed: the request must not be `initialize` (which
    // resets the server's negotiation and suppresses the disclosure entirely), and it
    // must not re-send `explicit_gating` (which the client's own latch guarantees, so
    // this test is coupled to the latch working).
    let method = "tools/list";
    assert_ne!(
        method, "initialize",
        "an initialize would reset the server's negotiation"
    );
    round_trip(&mut fx, "down-2", method).await;
    let second = client_request_event(&fx.pool, fx.client_pubkey, "down-2").await;
    assert!(
        !has_mode_tag(&second),
        "the follow-up must not re-send the mode, or the server would simply re-reject it"
    );

    assert_eq!(
        fx.client.get_effective_payment_interaction(),
        Some(PaymentInteractionMode::Transparent),
        "the client records the OBSERVED downgrade, not the mode it requested"
    );
}

/// Emission and learning both survive gift wrap: the tags ride the decrypted inner
/// event on the way out, and the learner reads the inner event's tags on the way in.
/// Wiring the tags onto the outer wrap instead is an easy mistake to make, and no other
/// test here would catch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negotiation_tags_survive_gift_wrap() {
    let mut fx = fixture_with_encryption(
        EncryptionMode::Required,
        |c| {
            c.with_pmis(vec!["lightning".to_string()])
                .with_payment_interaction(PaymentInteractionMode::ExplicitGating)
        },
        |s| s.set_supported_payment_interaction(PaymentInteractionPolicy::Optional),
    )
    .await;

    fx.client
        .send(&request("wrap-1", "tools/list"))
        .await
        .unwrap();
    let incoming = tokio::time::timeout(Duration::from_millis(1000), fx.server_rx.recv())
        .await
        .expect("encrypted request must reach the server")
        .expect("server channel closed");

    // The server parsed the tags off the decrypted inner event, which is the claim: a
    // client that wired them onto the outer gift wrap would negotiate nothing.
    let snap = fx
        .server
        .session_snapshot(&incoming.client_pubkey)
        .await
        .expect("server session");
    assert_eq!(
        snap.requested_payment_interaction,
        Some(PaymentInteractionMode::ExplicitGating),
        "the negotiation tags must ride the inner event under gift wrap"
    );

    fx.server
        .send_response(
            &incoming.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("wrap-1"),
                result: serde_json::json!({ "content": [] }),
            }),
        )
        .await
        .expect("send response");
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        fx.client.get_effective_payment_interaction(),
        Some(PaymentInteractionMode::ExplicitGating),
        "and the learner must read the disclosure off the decrypted inner event"
    );
}

// ── Latch placement: which call site, and only after a successful publish ───
//
// The tests above pin the latch's VALUE semantics (which mode is emitted, and when).
// These pin its PLACEMENT semantics: that every publish path carries the emitted mode
// through to the flip, and that the flip happens only after the publish succeeded.
// A regression in either is invisible on the wire until a later request, so each path is
// driven end to end here rather than asserted through the getter.

/// A relay pool wrapper over `MockRelayPool` that can run a hook inside `publish_event`
/// before delegating, and can fail publishes on demand.
struct InterceptPool {
    inner: Arc<MockRelayPool>,
    /// Installed after `start()`. Called once, mid-publish, to drive a setter into the
    /// window between composing the tags and flipping the latch.
    setter_hook: Arc<OnceLock<Arc<NostrClientTransport>>>,
    hook_fired: AtomicBool,
    fail_publishes: Arc<AtomicBool>,
}

impl InterceptPool {
    fn new(
        inner: Arc<MockRelayPool>,
        setter_hook: Arc<OnceLock<Arc<NostrClientTransport>>>,
        fail_publishes: Arc<AtomicBool>,
    ) -> Self {
        Self {
            inner,
            setter_hook,
            hook_fired: AtomicBool::new(false),
            fail_publishes,
        }
    }
}

#[async_trait]
impl RelayPoolTrait for InterceptPool {
    async fn connect(&self, relay_urls: &[String]) -> contextvm_sdk::Result<()> {
        self.inner.connect(relay_urls).await
    }
    async fn disconnect(&self) -> contextvm_sdk::Result<()> {
        self.inner.disconnect().await
    }
    async fn publish_event(&self, event: &Event) -> contextvm_sdk::Result<EventId> {
        if let Some(transport) = self.setter_hook.get() {
            if !self.hook_fired.swap(true, Ordering::SeqCst) {
                transport.set_payment_interaction(PaymentInteractionMode::Transparent);
            }
        }
        if self.fail_publishes.load(Ordering::SeqCst) {
            return Err(contextvm_sdk::Error::Transport(
                "injected publish failure".to_string(),
            ));
        }
        self.inner.publish_event(event).await
    }
    async fn publish(&self, builder: EventBuilder) -> contextvm_sdk::Result<EventId> {
        self.inner.publish(builder).await
    }
    async fn sign(&self, builder: EventBuilder) -> contextvm_sdk::Result<Event> {
        self.inner.sign(builder).await
    }
    async fn signer(&self) -> contextvm_sdk::Result<Arc<dyn NostrSigner>> {
        self.inner.signer().await
    }
    fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
        self.inner.notifications()
    }
    async fn public_key(&self) -> contextvm_sdk::Result<PublicKey> {
        self.inner.public_key().await
    }
    async fn subscribe(&self, filters: Vec<Filter>) -> contextvm_sdk::Result<()> {
        self.inner.subscribe(filters).await
    }
    async fn publish_to(
        &self,
        urls: &[String],
        builder: EventBuilder,
    ) -> contextvm_sdk::Result<EventId> {
        self.inner.publish_to(urls, builder).await
    }
    async fn fetch_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
    ) -> contextvm_sdk::Result<Vec<Event>> {
        self.inner.fetch_events(filters, timeout).await
    }
}

/// Build a client over an `InterceptPool`, started and wrapped in an `Arc` so a hook can
/// call back into it. Returns the client, the shared relay store, and the client pubkey.
async fn intercepting_client(
    oversized: OversizedTransferConfig,
    setter_hook: Arc<OnceLock<Arc<NostrClientTransport>>>,
    fail_publishes: Arc<AtomicBool>,
) -> (Arc<NostrClientTransport>, Arc<MockRelayPool>, PublicKey) {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let pool = Arc::new(InterceptPool::new(
        Arc::new(client_pool),
        setter_hook,
        fail_publishes,
    ));
    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized)
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        pool as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create client transport");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    (Arc::new(client), server_pool, client_pubkey)
}

fn oversized_request(id: &str, blob_len: usize) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "_meta": { "progressToken": format!("tok-{id}") },
            "blob": "A".repeat(blob_len),
        })),
    })
}

/// An oversized-ELIGIBLE request that fits in one event still burns the latch.
///
/// This is the branch every rmcp `tools/call` carrying a progress token takes when
/// oversized transfer is enabled and the payload fits: the client measures the built
/// event, finds it under the threshold, and publishes it as a single event. No other test
/// reaches it, because the oversized tests use a payload that short-circuits on raw length
/// before any measuring happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_eligible_but_fitting_request_latches() {
    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(60_000)
        .with_chunk_size(60_000);
    let (client, pool, client_pubkey) = intercepting_client(
        oversized,
        Arc::new(OnceLock::new()),
        Arc::new(AtomicBool::new(false)),
    )
    .await;

    client.send(&oversized_request("fit-1", 10)).await.unwrap();
    client.send(&oversized_request("fit-2", 10)).await.unwrap();

    let first = client_request_event(&pool, client_pubkey, "fit-1").await;
    assert!(
        all_tags(&first).contains(&tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"])),
        "an oversized-eligible request that fits still carries the requested mode"
    );
    let second = client_request_event(&pool, client_pubkey, "fit-2").await;
    assert!(
        !has_mode_tag(&second),
        "and it must burn the latch, or the client re-sends the mode on every later request"
    );
}

/// A `set_payment_interaction` landing DURING the publish must not latch a mode that was
/// never on the wire.
///
/// The emitted mode is threaded by value from the tag composition to the flip, rather than
/// re-read at the flip site. Re-reading would be a second critical section, and this test
/// drives a setter straight into it: the relay pool calls the setter from inside
/// `publish_event`, which is the only await between the two points, so the interleaving is
/// deterministic rather than a timing race.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setter_during_publish_does_not_latch_an_unpublished_mode() {
    let hook: Arc<OnceLock<Arc<NostrClientTransport>>> = Arc::new(OnceLock::new());
    let (client, pool, client_pubkey) = intercepting_client(
        OversizedTransferConfig::default(),
        Arc::clone(&hook),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    // Installed only now: the hook needs the started transport, and nothing publishes
    // during `start()`.
    hook.set(Arc::clone(&client))
        .map_err(|_| "hook already set")
        .unwrap();

    // Request 1 composes `explicit_gating`; the pool flips the requested mode to
    // `transparent` mid-publish, after the tags are already built.
    client.send(&request("race-1", "tools/list")).await.unwrap();
    let first = client_request_event(&pool, client_pubkey, "race-1").await;
    assert!(
        all_tags(&first).contains(&tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"])),
        "request 1 published explicit_gating, whatever the setter did afterwards"
    );

    // The latch must record what was PUBLISHED, so `transparent` is still pending and
    // rides request 2. Latching the concurrently-set mode instead would drop it silently.
    client.send(&request("race-2", "tools/list")).await.unwrap();
    let second = client_request_event(&pool, client_pubkey, "race-2").await;
    assert!(
        all_tags(&second).contains(&tag(&[tags::PAYMENT_INTERACTION, "transparent"])),
        "the mode set during the publish was never published, so it must still be pending"
    );
}

/// A FAILED publish must leave the mode pending so it rides the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_publish_does_not_burn_the_latch() {
    let fail = Arc::new(AtomicBool::new(true));
    let (client, pool, client_pubkey) = intercepting_client(
        OversizedTransferConfig::default(),
        Arc::new(OnceLock::new()),
        Arc::clone(&fail),
    )
    .await;

    assert!(
        client.send(&request("fail-1", "tools/list")).await.is_err(),
        "the injected failure must surface as a send error"
    );

    fail.store(false, Ordering::SeqCst);
    client.send(&request("fail-2", "tools/list")).await.unwrap();
    let second = client_request_event(&pool, client_pubkey, "fail-2").await;
    assert!(
        all_tags(&second).contains(&tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"])),
        "a mode that never reached a relay must not be treated as published"
    );
}

/// The same rule on the oversized path: a transfer that fails leaves the mode pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_oversized_transfer_does_not_burn_the_latch() {
    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(6000)
        .with_chunk_size(6000)
        .with_accept_timeout_ms(200);
    let fail = Arc::new(AtomicBool::new(true));
    let (client, pool, client_pubkey) =
        intercepting_client(oversized, Arc::new(OnceLock::new()), Arc::clone(&fail)).await;

    assert!(
        client
            .send(&oversized_request("ofail-1", 10_000))
            .await
            .is_err(),
        "the injected failure must fail the oversized transfer"
    );

    fail.store(false, Ordering::SeqCst);
    client
        .send(&request("ofail-2", "tools/list"))
        .await
        .unwrap();
    let second = client_request_event(&pool, client_pubkey, "ofail-2").await;
    assert!(
        all_tags(&second).contains(&tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"])),
        "a failed oversized transfer must not burn the latch either"
    );
}

/// The borderline branch: raw payload fits under the threshold, but the SIGNED event does
/// not, so the client falls back to fragmenting after measuring.
///
/// The threshold is derived from the actual serialized request so the branch is taken by
/// construction rather than by a guessed constant, and the test asserts both halves of the
/// branch condition (raw content under, event fragmented anyway) so it fails loudly if the
/// arithmetic ever drifts into the plain oversized path instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn borderline_request_measured_over_threshold_latches() {
    let payload = oversized_request("border-1", 400);
    let raw_len = serde_json::to_string(&payload).expect("serialize").len();
    // Above the raw length (so the cheap check passes) but below the signed event, which
    // adds id, pubkey, sig, and tags.
    let threshold = raw_len + 40;

    let oversized = OversizedTransferConfig::enabled()
        .with_threshold(threshold)
        .with_chunk_size(threshold);
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized.clone()),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_oversized_transfer(oversized)
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        Arc::new(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    assert!(
        raw_len < threshold,
        "precondition: the raw payload must pass the cheap length check ({raw_len} < {threshold})"
    );
    client
        .send(&payload)
        .await
        .expect("send borderline request");

    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("borderline request must reassemble")
        .expect("server channel closed");
    assert_eq!(incoming.message.id(), Some(&serde_json::json!("border-1")));

    let events = server_pool.stored_events().await;
    let start = events
        .iter()
        .filter(|e| e.kind == Kind::Custom(CTXVM_MESSAGES_KIND) && e.pubkey == client_pubkey)
        .find(|e| {
            serde_json::from_str::<serde_json::Value>(&e.content)
                .ok()
                .as_ref()
                .and_then(|v| v.get("params"))
                .and_then(|p| p.get("cvm"))
                .and_then(|c| c.get("frameType"))
                .and_then(|f| f.as_str())
                == Some("start")
        })
        .expect("the measured event must have been fragmented, or this test drove another branch");

    assert!(
        all_tags(start).contains(&tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"])),
        "the negotiation set rides the start frame on the borderline path too"
    );

    client
        .send(&request("border-2", "tools/list"))
        .await
        .unwrap();
    let follow_up = client_request_event(&server_pool, client_pubkey, "border-2").await;
    assert!(
        !has_mode_tag(&follow_up),
        "and the borderline path must burn the latch like every other publish site"
    );
}

// ── Stateless explicit gating ───────────────────────────────────────────────

/// The explicit-gating lifecycle's first step, driven end to end: a stateless client's very
/// first request is answered with a payment-required error and nothing else.
///
/// The error is the only event the client ever sees, so it has to carry both the effective-mode
/// disclosure (or the client never learns the mode it negotiated) and the CEP-35 discovery set
/// (or the client latches this thin error as its session baseline and reports no server identity
/// for the rest of the session, because the baseline only ever upgrades to an event carrying a
/// full initialize result).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stateless_gating_error_discloses_and_replays_discovery() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default()
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_server_info(ServerInfo::default().with_name("Gating-Server".to_string())),
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");
    server.set_supported_payment_interaction(PaymentInteractionPolicy::Optional);

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_payment_interaction(PaymentInteractionMode::ExplicitGating),
        Arc::new(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    client
        .send(&request("gated-1", "tools/call"))
        .await
        .expect("send the gated request");
    let incoming = tokio::time::timeout(Duration::from_millis(1000), server_rx.recv())
        .await
        .expect("request must reach the server")
        .expect("server channel closed");

    // The gate answers the request without ending it, exactly as a middleware would.
    let client_pubkey_hex = incoming.client_pubkey.clone();
    server
        .send_targeted_response(
            &client_pubkey_hex,
            &incoming.event_id,
            JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("gated-1"),
                error: JsonRpcError {
                    code: -32042,
                    message: "Payment required".to_string(),
                    data: None,
                },
            }),
        )
        .await
        .expect("targeted send");
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Server side: the whole tag list, in composition order.
    let published = server_pool.stored_events().await;
    let error_event = published
        .iter()
        .find(|e| {
            e.kind == Kind::Custom(CTXVM_MESSAGES_KIND)
                && e.pubkey == server_pubkey
                && e.content.contains("-32042")
        })
        .expect("the gating error must be published");
    assert_eq!(
        all_tags(error_event),
        vec![
            tag(&["p", &client_pubkey_hex]),
            tag(&["e", &incoming.event_id]),
            tag(&["name", "Gating-Server"]),
            tag(&[tags::SUPPORT_OVERSIZED_TRANSFER]),
            tag(&[tags::PAYMENT_INTERACTION, "explicit_gating"]),
        ],
        "the gating error must carry routing, the discovery replay and the disclosure"
    );

    // Client side: the consequence of both halves.
    assert_eq!(
        client.get_effective_payment_interaction(),
        Some(PaymentInteractionMode::ExplicitGating),
        "the client must learn the effective mode from the error"
    );
    let baseline = client
        .get_server_initialize_event()
        .expect("the error is the client's first discovery-carrying event, so it is the baseline");
    assert!(
        all_tags(&baseline)
            .iter()
            .any(|t| t.first().map(String::as_str) == Some("name")),
        "the baseline must carry the server's identity, not just the routing tags: without the \
         discovery replay this session would report no server name at all"
    );
    assert!(
        client
            .discovered_server_capabilities()
            .supports_oversized_transfer,
        "the discovery set must have been learned from the error event"
    );

    server.close().await.expect("close server");
    client.close().await.expect("close client");
}
