//! The NWC rail end to end over one `MockRelayPool` group: real client and
//! server transports, registered through the production payment entry points,
//! with a scripted NIP-47 wallet on each side.
//!
//! Nothing here stubs the rail. The server's `LnBolt11NwcPaymentProcessor`
//! really mints an invoice by publishing a kind-23194 `make_invoice` to its
//! wallet and reading the kind-23195 answer; the client's
//! `LnBolt11NwcPaymentHandler` really pays by publishing `pay_invoice` to its
//! own wallet. Settlement is made causal rather than assumed: the server's
//! wallet answers `lookup_invoice` as pending until the client's wallet has
//! actually been asked to pay, and only then flips to settled.
//!
//! Clock discipline follows `payments_bilateral_e2e`: real clock with small
//! configured timeouts, because the correlation sweep and session expiry age on
//! `std::time::Instant`, which paused tokio time does not advance.

use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::payments::nip47::mock_wallet::{MockWallet, WalletBehavior};
use contextvm_sdk::payments::nip47::{parse_nwc_uri, NwcClient, NwcClientOptions};
use contextvm_sdk::payments::traits::PaymentHandler;
use contextvm_sdk::payments::types::PaymentHandlerRequest;
use contextvm_sdk::payments::types::PricedCapability;
use contextvm_sdk::payments::{
    with_client_payments, with_server_payments, ClientPaymentsOptions, LnBolt11NwcPaymentHandler,
    LnBolt11NwcPaymentProcessor, LnBolt11NwcPaymentProcessorOptions, OnPaymentRequiredFn,
    PaymentApproval, PaymentPolicyFn, ServerPaymentsOptions,
};
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{
    IncomingRequest, NostrServerTransport, NostrServerTransportConfig,
};
use contextvm_sdk::{
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, PaymentInteractionMode, RelayPoolTrait,
};
use futures::FutureExt;
use nostr_sdk::prelude::*;

const INVOICE: &str = "lnbc210n1p-e2e";
const PAYMENT_HASH: &str = "hash-e2e";

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

fn priced() -> PricedCapability {
    PricedCapability {
        method: "tools/call".to_string(),
        name: Some("paid-tool".to_string()),
        amount: 21,
        currency_unit: "sats".to_string(),
        max_amount: None,
        description: None,
    }
}

/// Next message off the client channel, skipping the `payment_required`
/// notification the engine forwards to the consumer before the outcome.
async fn recv_outcome_within(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    ms: u64,
    what: &str,
) -> JsonRpcMessage {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let msg = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("client channel closed");
        match msg {
            JsonRpcMessage::Notification(ref n)
                if n.method.starts_with("notifications/payment")
                    || n.method == "notifications/progress" => {}
            other => return other,
        }
    }
}

/// Build a NWC client over `pool` talking to `wallet`.
fn nwc_client_for(
    pool: &Arc<MockRelayPool>,
    wallet: &MockWallet,
    response_timeout: Duration,
) -> Arc<NwcClient> {
    let secret = SecretKey::generate();
    let uri = format!(
        "nostr+walletconnect://{}?relay=wss%3A%2F%2Fmock.relay&secret={}",
        wallet.public_key().to_hex(),
        secret.to_secret_hex()
    );
    let connection = parse_nwc_uri(&uri).expect("uri parses");
    Arc::new(
        NwcClient::with_pool(
            as_pool(pool),
            connection,
            NwcClientOptions::new().with_response_timeout(response_timeout),
        )
        .expect("options valid"),
    )
}

struct Fx {
    server: NostrServerTransport,
    server_rx: tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
    client: NostrClientTransport,
    client_rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    /// The wallet the server mints and checks invoices against.
    server_wallet: Arc<MockWallet>,
    /// The wallet the client pays from.
    client_wallet: Arc<MockWallet>,
    /// The same handler the engine holds, so an explicit-gating callback can
    /// drive the rail by hand.
    handler: Arc<LnBolt11NwcPaymentHandler>,
}

/// A paired client and server on one mock relay group, each with its own
/// scripted wallet, both registered through the production entry points.
async fn fixture(
    mode: PaymentInteractionMode,
    payment_policy: Option<Arc<PaymentPolicyFn>>,
    pay_behavior: WalletBehavior,
    on_payment_required: Option<Arc<OnPaymentRequiredFn>>,
) -> Fx {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);
    let client_pool = Arc::new(client_pool);

    // Both wallets live on the same shared store; each filters by kind and
    // `p` tag, so they never answer for one another.
    let server_wallet_pool = Arc::new(server_pool.linked_with_keys(Keys::generate()));
    let client_wallet_pool = Arc::new(server_pool.linked_with_keys(Keys::generate()));
    let server_wallet = Arc::new(MockWallet::new(Arc::clone(&server_wallet_pool)));
    let client_wallet = Arc::new(MockWallet::new(Arc::clone(&client_wallet_pool)));
    server_wallet.start().await.expect("server wallet starts");
    client_wallet.start().await.expect("client wallet starts");

    server_wallet
        .set_behavior(
            "make_invoice",
            WalletBehavior::Answer(serde_json::json!({
                "invoice": INVOICE,
                "payment_hash": PAYMENT_HASH,
                "state": "pending",
            })),
        )
        .await;
    // Pending until the client has actually paid; the test flips this.
    server_wallet
        .set_behavior(
            "lookup_invoice",
            WalletBehavior::Answer(serde_json::json!({ "state": "pending" })),
        )
        .await;
    client_wallet
        .set_behavior("pay_invoice", pay_behavior)
        .await;

    let processor = Arc::new(LnBolt11NwcPaymentProcessor::with_client(
        nwc_client_for(&server_pool, &server_wallet, Duration::from_millis(800)),
        LnBolt11NwcPaymentProcessorOptions::new()
            .with_ttl(Duration::from_secs(4))
            .with_poll_interval(Duration::from_millis(40))
            .with_response_timeout(Duration::from_millis(800))
            .with_notification_verification(false),
    ));
    let handler = Arc::new(LnBolt11NwcPaymentHandler::with_client(nwc_client_for(
        &client_pool,
        &client_wallet,
        Duration::from_millis(800),
    )));
    let gating_handler = Arc::clone(&handler);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig::default().with_encryption_mode(EncryptionMode::Disabled),
        as_pool(&server_pool),
    )
    .await
    .expect("server transport");
    with_server_payments(
        &mut server,
        ServerPaymentsOptions::new(vec![processor], vec![priced()])
            .with_payment_ttl(Duration::from_secs(4)),
    )
    .expect("register server payments");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_timeout(Duration::from_secs(30)),
        as_pool(&client_pool),
    )
    .await
    .expect("client transport");
    with_client_payments(&mut client, {
        let mut opts = ClientPaymentsOptions::new()
            .with_handlers(vec![handler])
            .with_payment_interaction(mode)
            .with_synthetic_progress_interval(Duration::from_millis(400));
        if let Some(policy) = payment_policy {
            opts = opts.with_payment_policy(policy);
        }
        if let Some(cb) = on_payment_required {
            opts = opts.with_on_payment_required(cb);
        }
        opts
    })
    .expect("register client payments");

    let server_rx = server.take_message_receiver().expect("server rx");
    let client_rx = client.take_message_receiver().expect("client rx");
    server.start().await.expect("server start");
    client.start().await.expect("client start");
    tokio::time::sleep(Duration::from_millis(30)).await;

    Fx {
        server,
        server_rx,
        client,
        client_rx,
        server_wallet,
        client_wallet,
        handler: gating_handler,
    }
}

fn paid_answer() -> WalletBehavior {
    WalletBehavior::Answer(serde_json::json!({ "preimage": "pre-e2e", "fees_paid": 1 }))
}

/// Flip the server's wallet to settled as soon as the client's wallet has
/// actually been asked to pay, so settlement is caused by the payment rather
/// than scheduled alongside it.
fn settle_after_payment(fx: &Fx) -> tokio::task::JoinHandle<bool> {
    let client_wallet = Arc::clone(&fx.client_wallet);
    let server_wallet = Arc::clone(&fx.server_wallet);
    tokio::spawn(async move {
        for _ in 0..200 {
            if client_wallet.call_count("pay_invoice").await > 0 {
                server_wallet
                    .set_behavior(
                        "lookup_invoice",
                        WalletBehavior::Answer(serde_json::json!({
                            "state": "settled",
                            "payment_hash": PAYMENT_HASH,
                        })),
                    )
                    .await;
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    })
}

// ── transparent lifecycle ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn transparent_lifecycle_mints_pays_and_delivers() {
    let mut fx = fixture(
        PaymentInteractionMode::Transparent,
        None,
        paid_answer(),
        None,
    )
    .await;
    let settler = settle_after_payment(&fx);

    fx.client
        .send(&paid_call("req-1"))
        .await
        .expect("priced call sent");

    // The server forwards only after its own wallet confirms settlement.
    let forwarded = tokio::time::timeout(Duration::from_secs(8), fx.server_rx.recv())
        .await
        .expect("the paid call should reach the handler")
        .expect("server channel open");

    assert!(settler.await.expect("settler task"), "the client did pay");
    assert_eq!(fx.client_wallet.call_count("pay_invoice").await, 1);
    assert_eq!(fx.server_wallet.call_count("make_invoice").await, 1);
    assert!(fx.server_wallet.call_count("lookup_invoice").await >= 1);

    // The invoice the client paid is the one the server minted.
    let paid = fx.client_wallet.calls().await;
    let pay = paid.iter().find(|(m, _)| m == "pay_invoice").unwrap();
    assert_eq!(pay.1["invoice"].as_str(), Some(INVOICE));

    // Answer it so delivery can be asserted at the client's own channel.
    fx.server
        .send_response(
            &forwarded.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("req-1"),
                result: serde_json::json!({ "ok": true }),
            }),
        )
        .await
        .expect("response sent");

    let delivered = recv_outcome_within(&mut fx.client_rx, 4000, "the paid result").await;
    match delivered {
        JsonRpcMessage::Response(r) => assert_eq!(r.result, serde_json::json!({ "ok": true })),
        other => panic!("expected the paid result, got {other:?}"),
    }
}

/// A declining policy must stop before the wallet is touched.
#[tokio::test(flavor = "multi_thread")]
async fn a_declining_policy_never_calls_the_wallet() {
    // PaymentPolicyFn returns a bool; `false` declines and makes the engine
    // synthesize a -32000 toward the local consumer.
    let policy: Arc<PaymentPolicyFn> = Arc::new(|_req| async { false }.boxed());
    let mut fx = fixture(
        PaymentInteractionMode::Transparent,
        Some(policy),
        paid_answer(),
        None,
    )
    .await;

    fx.client
        .send(&paid_call("req-decline"))
        .await
        .expect("priced call sent");

    // The engine synthesizes an error for the caller rather than paying.
    let surfaced = recv_outcome_within(&mut fx.client_rx, 4000, "the synthesized decline").await;
    match surfaced {
        JsonRpcMessage::ErrorResponse(e) => assert_eq!(e.error.code, -32000),
        other => panic!("expected a synthesized -32000, got {other:?}"),
    }

    assert_eq!(
        fx.client_wallet.call_count("pay_invoice").await,
        0,
        "a declined payment must never reach the wallet"
    );
    // The server still minted an invoice: the decline is client-side.
    assert_eq!(fx.server_wallet.call_count("make_invoice").await, 1);
    // And nothing was forwarded to the handler.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), fx.server_rx.recv())
            .await
            .is_err(),
        "a declined payment must not be forwarded"
    );
}

/// A wallet that refuses to pay leaves the request pending: no acceptance, no
/// forward, and nothing synthesized onto the caller's id by the handler itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_pay_invoice_error_leaves_the_request_pending() {
    let mut fx = fixture(
        PaymentInteractionMode::Transparent,
        None,
        WalletBehavior::Error {
            code: "INSUFFICIENT_BALANCE".to_string(),
            message: "no sats".to_string(),
        },
        None,
    )
    .await;

    fx.client
        .send(&paid_call("req-nofunds"))
        .await
        .expect("priced call sent");
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(
        fx.client_wallet.call_count("pay_invoice").await,
        1,
        "the wallet was asked once and refused"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), fx.server_rx.recv())
            .await
            .is_err(),
        "an unpaid invoice must never be forwarded"
    );
}

/// A wallet that never settles must end at the TTL without the request being
/// forwarded: the verify times out rather than being read as payment.
#[tokio::test(flavor = "multi_thread")]
async fn a_never_settling_wallet_ends_at_the_ttl_with_no_acceptance() {
    // `pay_invoice` succeeds, but the server's wallet stays pending forever.
    let mut fx = fixture(
        PaymentInteractionMode::Transparent,
        None,
        paid_answer(),
        None,
    )
    .await;

    fx.client
        .send(&paid_call("req-stuck"))
        .await
        .expect("priced call sent");

    // The configured payment TTL is 4 s; give it room and assert nothing
    // reached the handler.
    assert!(
        tokio::time::timeout(Duration::from_secs(6), fx.server_rx.recv())
            .await
            .is_err(),
        "a never-settling invoice must not be forwarded"
    );
    assert!(fx.client_wallet.call_count("pay_invoice").await >= 1);
    assert!(
        fx.server_wallet.call_count("lookup_invoice").await >= 2,
        "the processor should have polled repeatedly before giving up"
    );
}

// ── explicit gating ──────────────────────────────────────────────────────────

/// Explicit gating answers `-32042`, the consumer pays through the rail in an
/// `on_payment_required` callback, and the engine retries to claim the grant.
///
/// The NWC handler is NOT auto-invoked in this mode: the engine routes
/// `-32042` to `on_payment_required`, and `PaymentHandler` is the transparent
/// lifecycle's hook. The callback here drives the same handler by hand, which
/// is how a consumer wires this rail for gating.
#[tokio::test(flavor = "multi_thread")]
async fn explicit_gating_pays_through_the_rail_and_the_retry_delivers() {
    // Set after the fixture is built, so the callback can hold the same
    // handler instance the engine was registered with.
    let shared: Arc<tokio::sync::OnceCell<Arc<LnBolt11NwcPaymentHandler>>> =
        Arc::new(tokio::sync::OnceCell::new());
    let cb_handle = Arc::clone(&shared);

    let callback: Arc<OnPaymentRequiredFn> = Arc::new(move |params| {
        let cb_handle = Arc::clone(&cb_handle);
        async move {
            let offer = params.options.first().expect("at least one option").clone();
            let handler = cb_handle.get().expect("handler wired").clone();
            handler
                .handle(PaymentHandlerRequest {
                    amount: offer.amount,
                    pay_req: offer.pay_req,
                    pmi: offer.pmi,
                    description: offer.description,
                    ttl: offer.ttl,
                    meta: None,
                    request_event_id: String::new(),
                })
                .await?;
            // Past the second boundary: a byte-identical retry inside the same
            // wall-clock second mints the same Nostr event id, which the relay
            // and the server's ingestion dedup both swallow.
            tokio::time::sleep(Duration::from_millis(1100)).await;
            Ok(PaymentApproval {
                paid: true,
                reason: None,
            })
        }
        .boxed()
    });

    let mut fx = fixture(
        PaymentInteractionMode::ExplicitGating,
        None,
        paid_answer(),
        Some(callback),
    )
    .await;
    // `OnceCell::set` returns the value back on error and the handler is not
    // `Debug`, so assert on `is_ok()` rather than unwrapping.
    assert!(
        shared.set(Arc::clone(&fx.handler)).is_ok(),
        "handler wired exactly once"
    );
    let settler = settle_after_payment(&fx);

    fx.client
        .send(&paid_call("req-gated"))
        .await
        .expect("priced call sent");

    let forwarded = tokio::time::timeout(Duration::from_secs(15), fx.server_rx.recv())
        .await
        .expect("the retry should reach the handler after the grant")
        .expect("server channel open");

    assert!(settler.await.expect("settler task"), "the client did pay");
    assert_eq!(fx.client_wallet.call_count("pay_invoice").await, 1);

    fx.server
        .send_response(
            &forwarded.event_id,
            JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!("req-gated"),
                result: serde_json::json!({ "gated": true }),
            }),
        )
        .await
        .expect("response sent");

    let delivered = recv_outcome_within(&mut fx.client_rx, 6000, "the gated result").await;
    match delivered {
        JsonRpcMessage::Response(r) => {
            assert_eq!(r.result, serde_json::json!({ "gated": true }))
        }
        // -32042/-32043 must never surface on the happy path.
        JsonRpcMessage::ErrorResponse(e) => {
            panic!("a gating error surfaced to the caller: {:?}", e.error)
        }
        other => panic!("expected the gated result, got {other:?}"),
    }
}
