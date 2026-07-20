//! CEP-8 payment traits: [`PaymentProcessor`] (server-side issue/verify),
//! [`PaymentHandler`] (client-side wallet), and [`ResolvePrice`] (server-side
//! dynamic pricing).
//!
//! All three are `Send + Sync` under `#[async_trait]` and object-safe (used as
//! `Arc<dyn _>` on a detached async task, which requires
//! `Send + Sync + 'static`). Fallible methods return `Result<_, PaymentError>`.

use async_trait::async_trait;

use crate::payments::errors::PaymentError;
use crate::payments::types::{
    PaymentHandlerRequest, PaymentProcessorCreateParams, PaymentProcessorVerifyParams,
    PaymentRequiredParams, ResolvePriceParams, ResolvePriceResult, VerifyOutcome,
};

/// Server-side module that issues and verifies payments for one PMI.
#[async_trait]
pub trait PaymentProcessor: Send + Sync {
    /// The PMI this processor issues/verifies.
    fn pmi(&self) -> &str;

    /// Create a payment request for a specific invocation.
    async fn create_payment_required(
        &self,
        params: PaymentProcessorCreateParams,
    ) -> Result<PaymentRequiredParams, PaymentError>;

    /// Wait for / verify settlement for a previously issued `pay_req`.
    async fn verify_payment(
        &self,
        params: PaymentProcessorVerifyParams,
    ) -> Result<VerifyOutcome, PaymentError>;
}

/// Client-side module that pays a single PMI in-band (wallet handler).
#[async_trait]
pub trait PaymentHandler: Send + Sync {
    /// The PMI this handler supports.
    fn pmi(&self) -> &str;

    /// Optional policy gate; the default accepts every request. (TS `canHandle?`.)
    async fn can_handle(&self, _req: &PaymentHandlerRequest) -> bool {
        true
    }

    /// Execute the payment (wallet action).
    async fn handle(&self, req: PaymentHandlerRequest) -> Result<(), PaymentError>;
}

/// Server-side dynamic-pricing callback. `cap` tags are discovery; this decides the final quote.
#[async_trait]
pub trait ResolvePrice: Send + Sync {
    /// Resolve the price (or reject/waive) for a priced invocation.
    ///
    /// Fallible so a resolver doing I/O (a price lookup, a balance check) can
    /// surface an operational failure via `Err`, kept distinct from a business
    /// decision to refuse service ([`ResolvePriceResult::Reject`]). This mirrors
    /// ts `ResolvePriceFn`, whose `Promise` may reject; the payment middleware
    /// treats an `Err` as a server-side failure, not a client refusal.
    async fn resolve_price(
        &self,
        params: ResolvePriceParams,
    ) -> Result<ResolvePriceResult, PaymentError>;
}
