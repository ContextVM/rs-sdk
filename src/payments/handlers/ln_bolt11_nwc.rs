//! CEP-8 payment handler for `bitcoin-lightning-bolt11`, backed by NIP-47 (NWC).
//!
//! Client side of the first Phase B rail: given a `payment_required` offer, pay
//! its BOLT11 invoice with `pay_invoice`.
//!
//! # This type does not decide whether to pay
//!
//! The spending decision belongs to `payment_policy` in
//! [`ClientPaymentsOptions`](crate::payments::ClientPaymentsOptions), which the
//! client engine consults *before* calling [`PaymentHandler::handle`]. This
//! handler deliberately leaves [`PaymentHandler::can_handle`] at its default
//! `true` and pays whatever reaches it. Putting a second spending gate here
//! would split the budget across two places, and the engine's gate is the one
//! that can also synthesize a `-32000` for the caller.
//!
//! # Bounds
//!
//! The client engine gives `handle()` neither a timeout nor a cancellation
//! token, so the only thing standing between a silent wallet and a wedged
//! payment task is the client's own `response_timeout`. That makes it part of
//! this rail's contract rather than a tuning knob.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::payments::constants::PMI_BITCOIN_LIGHTNING_BOLT11;
use crate::payments::errors::PaymentError;
use crate::payments::nip47::client::{NwcClient, NwcClientOptions, DEFAULT_RESPONSE_TIMEOUT};
use crate::payments::nip47::error::NwcError;
use crate::payments::nip47::uri::{parse_nwc_uri, NwcConnection};
use crate::payments::traits::PaymentHandler;
use crate::payments::types::PaymentHandlerRequest;
use crate::relay::RelayPoolTrait;

const LOG_TARGET: &str = "contextvm::payments::nwc_handler";

/// Construction options for [`LnBolt11NwcPaymentHandler`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LnBolt11NwcPaymentHandlerOptions {
    /// Per-payment wallet response timeout. Default 60 s.
    pub response_timeout: Duration,
}

impl Default for LnBolt11NwcPaymentHandlerOptions {
    fn default() -> Self {
        Self {
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
        }
    }
}

impl LnBolt11NwcPaymentHandlerOptions {
    /// Options with the default response timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the per-payment wallet response timeout.
    ///
    /// The client engine gives `handle()` no timeout of its own, so this is
    /// the only bound on a silent wallet.
    pub fn with_response_timeout(mut self, response_timeout: Duration) -> Self {
        self.response_timeout = response_timeout;
        self
    }
}

/// CEP-8 client handler for `bitcoin-lightning-bolt11` over NIP-47.
pub struct LnBolt11NwcPaymentHandler {
    client: Arc<NwcClient>,
    /// Mirrors the client's deadline, reused as the NIP-40 expiration.
    response_timeout: Duration,
}

impl LnBolt11NwcPaymentHandler {
    /// Build from a NWC connection string and a relay pool.
    ///
    /// # Errors
    ///
    /// Returns [`PaymentError::Processor`] when the connection string does not parse.
    pub fn from_uri(
        pool: Arc<dyn RelayPoolTrait>,
        uri: &str,
        options: LnBolt11NwcPaymentHandlerOptions,
    ) -> Result<Self, PaymentError> {
        let connection = parse_nwc_uri(uri).map_err(NwcError::into_handler_error)?;
        Self::from_connection(pool, connection, options)
    }

    /// Build from an already-parsed connection.
    pub fn from_connection(
        pool: Arc<dyn RelayPoolTrait>,
        connection: NwcConnection,
        options: LnBolt11NwcPaymentHandlerOptions,
    ) -> Result<Self, PaymentError> {
        let client = NwcClient::with_pool(
            pool,
            connection,
            NwcClientOptions::new().with_response_timeout(options.response_timeout),
        )
        .map_err(NwcError::into_handler_error)?;
        Ok(Self::with_client_and_timeout(
            Arc::new(client),
            options.response_timeout,
        ))
    }

    /// Build around an already-constructed client, for tests and custom transports.
    pub fn with_client(client: Arc<NwcClient>) -> Self {
        Self::with_client_and_timeout(client, DEFAULT_RESPONSE_TIMEOUT)
    }

    /// Like [`Self::with_client`], but states the deadline the client was
    /// built with, so it can also be sent as the request's NIP-40 expiration.
    pub fn with_client_and_timeout(client: Arc<NwcClient>, response_timeout: Duration) -> Self {
        Self {
            client,
            response_timeout,
        }
    }
}

#[async_trait]
impl PaymentHandler for LnBolt11NwcPaymentHandler {
    fn pmi(&self) -> &str {
        PMI_BITCOIN_LIGHTNING_BOLT11
    }

    async fn handle(&self, req: PaymentHandlerRequest) -> Result<(), PaymentError> {
        // Never log the invoice: it is a bearer payment request.
        tracing::debug!(
            target: LOG_TARGET,
            request_event_id = %req.request_event_id,
            amount = req.amount,
            "paying a NWC invoice"
        );

        // A NIP-40 expiration set to our own deadline: `pay_invoice` is not
        // idempotent, and a wallet that honors the tag will not pay a request
        // we have already given up waiting for.
        let envelope = self
            .client
            .pay_invoice(&req.pay_req, Some(self.response_timeout))
            .await
            .map_err(NwcError::into_handler_error)?;

        if let Some(err) = envelope.error {
            return Err(NwcError::Wallet {
                method: "pay_invoice".to_string(),
                detail: err.describe(),
            }
            .into_handler_error());
        }

        // A wallet that answers with neither result nor error has told us
        // nothing; treating that as paid would let the engine report success
        // for a payment that may never have left.
        if envelope.result.is_none() {
            return Err(PaymentError::Handler(
                "NWC pay_invoice returned neither result nor error".to_string(),
            ));
        }

        tracing::debug!(
            target: LOG_TARGET,
            request_event_id = %req.request_event_id,
            "NWC invoice paid"
        );
        Ok(())
    }
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::payments::nip47::mock_wallet::{MockWallet, WalletBehavior};
    use crate::relay::MockRelayPool;
    use nostr_sdk::prelude::*;

    struct Harness {
        wallet: Arc<MockWallet>,
        handler: LnBolt11NwcPaymentHandler,
    }

    async fn harness(response_timeout: Duration) -> Harness {
        let wallet_pool = Arc::new(MockRelayPool::new());
        let client_pool = Arc::new(wallet_pool.linked_with_keys(Keys::generate()));
        let wallet = Arc::new(MockWallet::new(Arc::clone(&wallet_pool)));
        wallet.start().await.expect("wallet starts");

        let secret = SecretKey::generate();
        let uri = format!(
            "nostr+walletconnect://{}?relay=wss%3A%2F%2Fmock.relay&secret={}",
            wallet.public_key().to_hex(),
            secret.to_secret_hex()
        );
        let handler = LnBolt11NwcPaymentHandler::from_uri(
            Arc::clone(&client_pool) as Arc<dyn RelayPoolTrait>,
            &uri,
            LnBolt11NwcPaymentHandlerOptions { response_timeout },
        )
        .expect("uri parses");

        Harness { wallet, handler }
    }

    fn request(pay_req: &str) -> PaymentHandlerRequest {
        PaymentHandlerRequest {
            amount: 21,
            pay_req: pay_req.to_string(),
            pmi: PMI_BITCOIN_LIGHTNING_BOLT11.to_string(),
            description: Some("a priced tool call".to_string()),
            ttl: Some(300),
            meta: None,
            request_event_id: "evt-1".to_string(),
        }
    }

    #[tokio::test]
    async fn advertises_the_bolt11_pmi() {
        let h = harness(Duration::from_millis(500)).await;
        assert_eq!(h.handler.pmi(), PMI_BITCOIN_LIGHTNING_BOLT11);
    }

    #[tokio::test]
    async fn pays_the_invoice_and_passes_it_verbatim() {
        let h = harness(Duration::from_millis(600)).await;
        h.wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::json!({ "preimage": "pre", "fees_paid": 3 })),
            )
            .await;

        h.handler
            .handle(request("lnbc210n1p-exact"))
            .await
            .expect("payment succeeds");

        let calls = h.wallet.calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "pay_invoice");
        assert_eq!(calls[0].1["invoice"].as_str(), Some("lnbc210n1p-exact"));
    }

    /// `can_handle` stays at its default so the engine's `payment_policy`
    /// remains the single spending gate.
    #[tokio::test]
    async fn can_handle_defaults_to_true() {
        let h = harness(Duration::from_millis(500)).await;
        assert!(h.handler.can_handle(&request("lnbc1")).await);
    }

    #[tokio::test]
    async fn a_wallet_error_fails_the_payment() {
        let h = harness(Duration::from_millis(600)).await;
        h.wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Error {
                    code: "INSUFFICIENT_BALANCE".to_string(),
                    message: "not enough sats".to_string(),
                },
            )
            .await;

        let err = h
            .handler
            .handle(request("lnbc1"))
            .await
            .expect_err("a wallet error must fail the payment");
        assert!(matches!(err, PaymentError::Handler(_)));
        assert!(err.to_string().contains("INSUFFICIENT_BALANCE"));
    }

    /// A wallet that answers with neither result nor error must not read as paid.
    #[tokio::test]
    async fn an_empty_response_fails_rather_than_reporting_success() {
        let h = harness(Duration::from_millis(600)).await;
        h.wallet
            .set_behavior(
                "pay_invoice",
                WalletBehavior::Answer(serde_json::Value::Null),
            )
            .await;

        assert!(h.handler.handle(request("lnbc1")).await.is_err());
    }

    /// The engine gives `handle()` no timeout, so the handler's own
    /// `response_timeout` is the only bound on a silent wallet.
    #[tokio::test]
    async fn the_handlers_own_timeout_bounds_a_silent_wallet() {
        let h = harness(Duration::from_millis(250)).await;
        h.wallet
            .set_behavior("pay_invoice", WalletBehavior::Silence)
            .await;

        let started = tokio::time::Instant::now();
        let res = h.handler.handle(request("lnbc1")).await;
        assert!(
            res.is_err(),
            "a silent wallet must not hang the payment task"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must fail at the configured timeout, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_malformed_response_fails_the_payment() {
        let h = harness(Duration::from_millis(600)).await;
        h.wallet
            .set_behavior("pay_invoice", WalletBehavior::Malformed)
            .await;
        assert!(h.handler.handle(request("lnbc1")).await.is_err());
    }

    #[tokio::test]
    async fn rejects_a_malformed_connection_string() {
        let pool = Arc::new(MockRelayPool::new());
        assert!(LnBolt11NwcPaymentHandler::from_uri(
            pool as Arc<dyn RelayPoolTrait>,
            "https://not-nwc",
            LnBolt11NwcPaymentHandlerOptions::default(),
        )
        .is_err());
    }

    /// An invoice is a bearer payment request; no tracing call may bind it.
    #[test]
    fn no_tracing_call_carries_the_invoice() {
        let src = include_str!("ln_bolt11_nwc.rs");
        let impl_src = &src[..src
            .find("#[cfg(all(test, feature = \"test-utils\"))]")
            .unwrap_or(src.len())];

        let mut checked = 0usize;
        for (idx, _) in impl_src.match_indices("tracing::") {
            let rest = &impl_src[idx..];
            let end = rest.find(");").map(|e| e + 2).unwrap_or(rest.len());
            let call = &rest[..end];
            checked += 1;
            for banned in ["pay_req", "invoice", "preimage", "secret"] {
                let binds = call.match_indices(banned).any(|(i, _)| {
                    let preceded_by_ident = call[..i]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_');
                    if preceded_by_ident {
                        return false;
                    }
                    let after = call[i + banned.len()..].trim_start();
                    after.starts_with('=') && !after.starts_with("==")
                });
                assert!(!binds, "a tracing call binds `{banned}`:\n{call}");
            }
        }
        assert!(
            checked >= 2,
            "expected to scan both tracing calls, saw {checked}"
        );
    }
}
