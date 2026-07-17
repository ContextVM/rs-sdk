//! CEP-8 capability pricing and payment primitives.
//!
//! This is the pure foundation consumed by the later CEP-8 PRs: protocol
//! constants, `cap` / `pmi` / `payment_interaction` tag builders and parsers,
//! the wire notification params and explicit-gating error `data` types, the
//! [`PaymentProcessor`] / [`PaymentHandler`] / [`ResolvePrice`] traits, the
//! [`PaymentError`] taxonomy, and deterministic fakes behind the `test-utils`
//! feature. It carries no transport wiring, no network, no canonicalization,
//! and no authorization store (those arrive in later PRs).
//!
//! Constants and tag builders stay reachable under their module paths
//! ([`crate::payments::constants`] / [`crate::payments::tags`]); the wire/config
//! types, traits, and error are also re-exported at this module root for
//! ergonomic crate-level access.

pub mod constants;
pub mod errors;
pub mod tags;
pub mod traits;
pub mod types;

#[cfg(feature = "test-utils")]
pub mod fakes;

pub use errors::PaymentError;
pub use traits::{PaymentHandler, PaymentProcessor, ResolvePrice};
pub use types::{
    Meta, PaymentAcceptedParams, PaymentHandlerRequest, PaymentInteractionPolicy, PaymentOption,
    PaymentPendingErrorData, PaymentProcessorCreateParams, PaymentProcessorVerifyParams,
    PaymentRejectedParams, PaymentRequiredErrorData, PaymentRequiredParams, PricedCapability,
    ResolvePriceParams, ResolvePriceResult, VerifyOutcome,
};

#[cfg(feature = "test-utils")]
pub use fakes::{
    FakePaymentHandler, FakePaymentHandlerOptions, FakePaymentProcessor,
    FakePaymentProcessorOptions,
};
