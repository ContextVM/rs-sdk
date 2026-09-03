//! CEP-8 capability pricing and payment primitives.
//!
//! This is the pure foundation the rest of the CEP-8 payment stack builds on: protocol
//! constants, `cap` / `pmi` / `payment_interaction` tag builders and parsers,
//! the wire notification params and explicit-gating error `data` types, the
//! [`PaymentProcessor`] / [`PaymentHandler`] / [`ResolvePrice`] traits, the
//! [`PaymentError`] taxonomy, the canonical invocation identity used for
//! explicit-gating authorization matching, the bounded [`AuthorizationStore`] of
//! pending and granted authorizations, and deterministic fakes behind the
//! `test-utils` feature. The one piece of transport wiring here is the
//! [`with_server_payments`](crate::payments::server_transport_payments::with_server_payments)
//! registration entry point, which composes the middlewares, tags, and senders
//! onto a server transport.
//!
//! Constants and tag builders stay reachable under their module paths
//! ([`crate::payments::constants`] / [`crate::payments::tags`]); the wire/config
//! types, traits, and error are also re-exported at this module root for
//! ergonomic crate-level access.

pub mod authorization_store;
pub mod canonical;
pub mod constants;
pub mod errors;
pub mod server_explicit_gating;
pub mod server_payments;
pub(crate) mod server_payments_utils;
pub mod server_transport_payments;
pub mod tags;
pub mod traits;
pub mod types;

#[cfg(feature = "test-utils")]
pub mod fakes;

pub use authorization_store::{AuthorizationStore, ClaimOrPending};
pub use canonical::{
    compute_canonical_invocation_hash, compute_canonical_invocation_identity,
    CanonicalInvocationIdentity,
};
pub use errors::PaymentError;
pub use server_explicit_gating::{
    create_explicit_gating_middleware, ExplicitGatingMiddlewareParams,
};
pub use server_payments::{
    create_server_payments_middleware, ServerPaymentsMiddlewareParams, ServerPaymentsOptions,
};
pub use server_transport_payments::with_server_payments;
pub use traits::{PaymentHandler, PaymentProcessor, ResolvePrice};
pub use types::{
    Meta, PaymentAcceptedParams, PaymentHandlerRequest, PaymentInteractionPolicy, PaymentOption,
    PaymentPendingErrorData, PaymentProcessorCreateParams, PaymentProcessorVerifyParams,
    PaymentRejectedParams, PaymentRequiredErrorData, PaymentRequiredParams, PricedCapability,
    ResolvePriceParams, ResolvePriceResult, UnsupportedPaymentInteractionData, VerifyOutcome,
};

#[cfg(feature = "test-utils")]
pub use fakes::{
    FakePaymentHandler, FakePaymentHandlerOptions, FakePaymentProcessor,
    FakePaymentProcessorOptions,
};
