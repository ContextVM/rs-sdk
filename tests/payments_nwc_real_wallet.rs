//! The NWC rail against a **real** wallet. `#[ignore]`d and environment-gated.
//!
//! CI does not drive a real wallet: the relay-level mocks in
//! `payments_nwc_e2e` are the merge gate, and ts-sdk gates nothing on a wallet
//! either. These run by hand before the rail merges, and the result is recorded
//! in the PR with the invoice redacted.
//!
//! # Running
//!
//! Two wallets are needed, because one side issues and the other pays. Pointing
//! both at the same wallet makes it pay its own invoice, which most wallets
//! refuse and which proves nothing about the rail.
//!
//! ```sh
//! export CONTEXTVM_NWC_SERVER_URI='nostr+walletconnect://<pubkey>?relay=wss://...&secret=<hex>'
//! export CONTEXTVM_NWC_CLIENT_URI='nostr+walletconnect://<pubkey>?relay=wss://...&secret=<hex>'
//! cargo test --features nwc,test-utils --test payments_nwc_real_wallet -- --ignored --nocapture
//! ```
//!
//! Each test skips with a printed reason when its variables are unset, so a
//! bare `--ignored` run on a machine with no wallet is a no-op rather than a
//! failure.
//!
//! # These tests move real money
//!
//! The amount is deliberately tiny (1 sat by default, override with
//! `CONTEXTVM_NWC_AMOUNT_SATS`), but it is real. Nothing here is reversible.

use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::payments::nip47::{parse_nwc_uri, NwcClient, NwcClientOptions};
use contextvm_sdk::payments::types::{
    PaymentHandlerRequest, PaymentProcessorCreateParams, PaymentProcessorVerifyParams,
};
use contextvm_sdk::payments::{
    LnBolt11NwcPaymentHandler, LnBolt11NwcPaymentProcessor, LnBolt11NwcPaymentProcessorOptions,
    PaymentHandler, PaymentProcessor,
};
use tokio_util::sync::CancellationToken;

const SERVER_URI_VAR: &str = "CONTEXTVM_NWC_SERVER_URI";
const CLIENT_URI_VAR: &str = "CONTEXTVM_NWC_CLIENT_URI";
const AMOUNT_VAR: &str = "CONTEXTVM_NWC_AMOUNT_SATS";

/// Redact an invoice down to something safe to paste into a PR.
///
/// A BOLT11 string is a bearer payment request; the prefix and length are
/// enough to show a real invoice was produced.
fn redact(invoice: &str) -> String {
    let head: String = invoice.chars().take(12).collect();
    format!("{head}...<redacted, {} chars>", invoice.len())
}

fn amount_sats() -> i64 {
    std::env::var(AMOUNT_VAR)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1)
}

/// A live client that owns a pool built from the connection's own relays.
///
/// Uses `NwcClient::connect`, the production path: sharing a transport's pool
/// would publish ContextVM traffic to the wallet's relays and leave a
/// permanent wallet filter on the transport's.
async fn live_client(uri: &str, label: &str) -> Arc<NwcClient> {
    let connection = parse_nwc_uri(uri)
        .unwrap_or_else(|e| panic!("{label} connection string does not parse: {e}"));
    Arc::new(
        NwcClient::connect(
            connection,
            NwcClientOptions::new().with_response_timeout(Duration::from_secs(30)),
        )
        .await
        .unwrap_or_else(|e| panic!("{label} client could not connect: {e}")),
    )
}

fn env_pair() -> Option<(String, String)> {
    match (
        std::env::var(SERVER_URI_VAR).ok(),
        std::env::var(CLIENT_URI_VAR).ok(),
    ) {
        (Some(s), Some(c)) if !s.is_empty() && !c.is_empty() => Some((s, c)),
        _ => {
            eprintln!(
                "skipping: set {SERVER_URI_VAR} and {CLIENT_URI_VAR} to two distinct wallets"
            );
            None
        }
    }
}

/// Mirrors ts `nwc-integration.test.ts`: issue an invoice and look it up.
/// No money moves.
#[tokio::test]
#[ignore = "needs a real NWC wallet; see the module docs"]
async fn issues_and_looks_up_an_invoice_against_a_real_wallet() {
    let Some(server_uri) = std::env::var(SERVER_URI_VAR).ok().filter(|s| !s.is_empty()) else {
        eprintln!("skipping: set {SERVER_URI_VAR}");
        return;
    };

    let processor = LnBolt11NwcPaymentProcessor::with_client(
        live_client(&server_uri, "server").await,
        LnBolt11NwcPaymentProcessorOptions::new()
            .with_response_timeout(Duration::from_secs(30))
            .with_notification_verification(false),
    );

    let issued = processor
        .create_payment_required(PaymentProcessorCreateParams {
            amount: amount_sats(),
            description: Some("contextvm rs-sdk real-wallet check".to_string()),
            request_event_id: "real-wallet-issue".to_string(),
            client_pubkey: "real-wallet-client".to_string(),
        })
        .await
        .expect("a real wallet should mint an invoice");

    println!("issued invoice: {}", redact(&issued.pay_req));
    assert!(issued.pay_req.to_lowercase().starts_with("lnbc"));
    assert_eq!(issued.amount, amount_sats());

    // Unpaid, so the verify must NOT settle. Bound it tightly and expect the
    // cancellation error rather than a settlement.
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(6)).await;
        c.cancel();
    });
    let verdict = processor
        .verify_payment(PaymentProcessorVerifyParams {
            pay_req: issued.pay_req.clone(),
            request_event_id: "real-wallet-issue".to_string(),
            client_pubkey: "real-wallet-client".to_string(),
            cancel,
        })
        .await;
    assert!(
        verdict.is_err(),
        "an unpaid invoice must not verify as settled"
    );
    println!("unpaid verify correctly refused: {:?}", verdict.err());
}

/// Mirrors ts `nwc-paid-capability-e2e.test.ts`: one wallet issues, the other
/// pays, and the processor observes settlement. **This moves real money.**
#[tokio::test]
#[ignore = "needs two real NWC wallets and moves real money; see the module docs"]
async fn a_real_payment_settles_end_to_end() {
    let Some((server_uri, client_uri)) = env_pair() else {
        return;
    };

    let processor = LnBolt11NwcPaymentProcessor::with_client(
        live_client(&server_uri, "server").await,
        LnBolt11NwcPaymentProcessorOptions::new()
            .with_ttl(Duration::from_secs(120))
            .with_poll_interval(Duration::from_secs(1))
            .with_response_timeout(Duration::from_secs(30))
            .with_notification_verification(false),
    );
    let handler = LnBolt11NwcPaymentHandler::with_client(live_client(&client_uri, "client").await);

    let issued = processor
        .create_payment_required(PaymentProcessorCreateParams {
            amount: amount_sats(),
            description: Some("contextvm rs-sdk real-wallet e2e".to_string()),
            request_event_id: "real-wallet-e2e".to_string(),
            client_pubkey: "real-wallet-client".to_string(),
        })
        .await
        .expect("the issuing wallet should mint an invoice");
    println!("issued invoice: {}", redact(&issued.pay_req));

    // Start verifying before paying, the way the middleware does.
    let cancel = CancellationToken::new();
    let verify = {
        let pay_req = issued.pay_req.clone();
        let cancel = cancel.clone();
        async move {
            processor
                .verify_payment(PaymentProcessorVerifyParams {
                    pay_req,
                    request_event_id: "real-wallet-e2e".to_string(),
                    client_pubkey: "real-wallet-client".to_string(),
                    cancel,
                })
                .await
        }
    };

    let pay = async {
        handler
            .handle(PaymentHandlerRequest {
                amount: issued.amount,
                pay_req: issued.pay_req.clone(),
                pmi: issued.pmi.clone(),
                description: issued.description.clone(),
                ttl: issued.ttl,
                meta: None,
                request_event_id: "real-wallet-e2e".to_string(),
            })
            .await
    };

    let guard = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(90)).await;
        guard.cancel();
    });

    let (verified, paid) = tokio::join!(verify, pay);
    paid.expect("the paying wallet should settle the invoice");
    let outcome = verified.expect("the issuing wallet should observe settlement");

    let receipt = outcome
        .meta
        .as_ref()
        .and_then(|m| m.get("payment_hash"))
        .and_then(|v| v.as_str())
        .map(|h| h.chars().take(10).collect::<String>())
        .unwrap_or_default();
    println!("settled, receipt prefix: {receipt}...");
    assert!(outcome.meta.is_some(), "settlement must carry a receipt");
}
