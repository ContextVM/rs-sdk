//! Server-side CEP-8 payment processors: the real Lightning rails behind the
//! [`PaymentProcessor`](crate::payments::PaymentProcessor) trait.
//!
//! Each rail is feature-gated so a consumer that does not need it pays neither
//! the dependency nor the compile cost.

#[cfg(feature = "nwc")]
pub mod ln_bolt11_nwc;

#[cfg(feature = "nwc")]
pub use ln_bolt11_nwc::{LnBolt11NwcPaymentProcessor, LnBolt11NwcPaymentProcessorOptions};
