//! Deterministic in-memory fakes for the CEP-8 payment traits, behind the
//! `test-utils` feature. Behavior mirrors ts-sdk's `fake-payment-processor.ts`
//! / `fake-payment-handler.ts` on the happy path, with one enrichment:
//! [`FakePaymentProcessor::verify_payment`] selects on the cancellation token
//! and returns `Err` when cancelled, so verify-timeout and shutdown tests are
//! deterministic AND a cancelled verify can never be read as a
//! settlement (ts relies on the caller's `withTimeout` to reject instead).

use std::time::Duration;

use async_trait::async_trait;

use crate::payments::errors::PaymentError;
use crate::payments::traits::{PaymentHandler, PaymentProcessor};
use crate::payments::types::{
    Meta, PaymentHandlerRequest, PaymentProcessorCreateParams, PaymentProcessorVerifyParams,
    PaymentRequiredParams, VerifyOutcome,
};

/// Construction options for [`FakePaymentProcessor`] (mirrors ts `FakePaymentProcessorOptions`).
#[derive(Debug, Clone)]
pub struct FakePaymentProcessorOptions {
    /// The PMI this processor issues. Default `"fake"`.
    pub pmi: String,
    /// Artificial delay (ms) for settlement verification. Default `50`.
    pub verify_delay_ms: u64,
    /// Artificial delay (ms) for request creation. Default `0`.
    pub create_delay_ms: u64,
    /// Optional TTL (seconds) to include in `payment_required`. Default `None`.
    pub ttl: Option<u64>,
}

impl Default for FakePaymentProcessorOptions {
    fn default() -> Self {
        Self {
            pmi: "fake".to_string(),
            verify_delay_ms: 50,
            create_delay_ms: 0,
            ttl: None,
        }
    }
}

/// Fake processor: issues a deterministic `pay_req` and "settles" after a delay.
pub struct FakePaymentProcessor {
    pmi: String,
    verify_delay_ms: u64,
    create_delay_ms: u64,
    ttl: Option<u64>,
}

impl FakePaymentProcessor {
    /// Construct with defaults (pmi `"fake"`, verify 50 ms, create 0 ms, no ttl).
    pub fn new() -> Self {
        Self::with_options(FakePaymentProcessorOptions::default())
    }

    /// Construct from explicit options.
    pub fn with_options(opts: FakePaymentProcessorOptions) -> Self {
        Self {
            pmi: opts.pmi,
            verify_delay_ms: opts.verify_delay_ms,
            create_delay_ms: opts.create_delay_ms,
            ttl: opts.ttl,
        }
    }
}

impl Default for FakePaymentProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PaymentProcessor for FakePaymentProcessor {
    fn pmi(&self) -> &str {
        &self.pmi
    }

    async fn create_payment_required(
        &self,
        p: PaymentProcessorCreateParams,
    ) -> Result<PaymentRequiredParams, PaymentError> {
        if self.create_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.create_delay_ms)).await;
        }
        Ok(PaymentRequiredParams {
            amount: p.amount,
            pay_req: format!(
                "fake:{}:{}:{}",
                p.request_event_id, p.client_pubkey, p.amount
            ),
            pmi: self.pmi.clone(),
            description: p.description,
            ttl: self.ttl,
            meta: None,
        })
    }

    async fn verify_payment(
        &self,
        p: PaymentProcessorVerifyParams,
    ) -> Result<VerifyOutcome, PaymentError> {
        // Honor cancellation so timeout/shutdown tests are deterministic.
        // A cancelled verify is a FAILURE, not an empty settlement: return Err so
        // it can never be read as settled (mirrors ts's `withTimeout` rejection).
        tokio::select! {
            _ = p.cancel.cancelled() => Err(PaymentError::Processor(
                "verify_payment cancelled".to_string(),
            )),
            _ = tokio::time::sleep(Duration::from_millis(self.verify_delay_ms)) => {
                let mut meta = Meta::new();
                meta.insert("settled".to_string(), serde_json::Value::Bool(true));
                Ok(VerifyOutcome { meta: Some(meta) })
            }
        }
    }
}

/// Construction options for [`FakePaymentHandler`] (mirrors ts `FakePaymentHandlerOptions`).
#[derive(Debug, Clone)]
pub struct FakePaymentHandlerOptions {
    /// The PMI this handler advertises. Default `"fake"`.
    pub pmi: String,
    /// Artificial delay (ms) simulating a wallet action. Default `50`.
    pub delay_ms: u64,
}

impl Default for FakePaymentHandlerOptions {
    fn default() -> Self {
        Self {
            pmi: "fake".to_string(),
            delay_ms: 50,
        }
    }
}

/// Fake handler: simulates a wallet action with a delay. Publishes no protocol messages.
pub struct FakePaymentHandler {
    pmi: String,
    delay_ms: u64,
}

impl FakePaymentHandler {
    /// Construct with defaults (pmi `"fake"`, 50 ms delay).
    pub fn new() -> Self {
        Self::with_options(FakePaymentHandlerOptions::default())
    }

    /// Construct from explicit options.
    pub fn with_options(opts: FakePaymentHandlerOptions) -> Self {
        Self {
            pmi: opts.pmi,
            delay_ms: opts.delay_ms,
        }
    }
}

impl Default for FakePaymentHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PaymentHandler for FakePaymentHandler {
    fn pmi(&self) -> &str {
        &self.pmi
    }

    async fn handle(&self, _req: PaymentHandlerRequest) -> Result<(), PaymentError> {
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn create_params(amount: i64) -> PaymentProcessorCreateParams {
        PaymentProcessorCreateParams {
            amount,
            description: Some("d".to_string()),
            request_event_id: "evt".to_string(),
            client_pubkey: "pk".to_string(),
        }
    }

    fn verify_params(cancel: CancellationToken) -> PaymentProcessorVerifyParams {
        PaymentProcessorVerifyParams {
            pay_req: "r".to_string(),
            request_event_id: "evt".to_string(),
            client_pubkey: "pk".to_string(),
            cancel,
        }
    }

    #[tokio::test]
    async fn processor_create_payment_required_builds_pay_req_and_passes_ttl() {
        let proc = FakePaymentProcessor::with_options(FakePaymentProcessorOptions {
            ttl: Some(600),
            create_delay_ms: 0,
            ..Default::default()
        });
        let out = proc
            .create_payment_required(create_params(42))
            .await
            .unwrap();
        assert_eq!(out.pay_req, "fake:evt:pk:42");
        assert_eq!(out.amount, 42);
        assert_eq!(out.pmi, "fake");
        assert_eq!(out.ttl, Some(600));
        assert_eq!(out.description.as_deref(), Some("d"));
        assert!(out.meta.is_none());
    }

    #[tokio::test]
    async fn processor_verify_payment_returns_settled_meta() {
        let proc = FakePaymentProcessor::with_options(FakePaymentProcessorOptions {
            verify_delay_ms: 0,
            ..Default::default()
        });
        let out = proc
            .verify_payment(verify_params(CancellationToken::new()))
            .await
            .unwrap();
        let meta = out.meta.expect("settled meta present");
        assert_eq!(meta.get("settled"), Some(&serde_json::Value::Bool(true)));
    }

    #[tokio::test]
    async fn processor_verify_payment_errors_on_pre_cancelled_token() {
        // Default 50 ms verify delay; a pre-cancelled token wins the select. A
        // cancelled verify is a failure, never a (empty) settlement.
        let proc = FakePaymentProcessor::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = proc
            .verify_payment(verify_params(cancel))
            .await
            .expect_err("a cancelled verify must not settle");
        assert!(matches!(err, PaymentError::Processor(_)));
    }

    #[tokio::test]
    async fn handler_handles_and_default_can_handle_is_true() {
        let handler = FakePaymentHandler::with_options(FakePaymentHandlerOptions {
            delay_ms: 0,
            ..Default::default()
        });
        let req = PaymentHandlerRequest {
            amount: 1,
            pay_req: "r".to_string(),
            pmi: "fake".to_string(),
            description: None,
            ttl: None,
            meta: None,
            request_event_id: "evt".to_string(),
        };
        assert_eq!(handler.pmi(), "fake");
        assert!(handler.can_handle(&req).await);
        handler.handle(req).await.unwrap();
    }
}
