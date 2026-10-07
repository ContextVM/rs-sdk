//! Client-side CEP-8 payment handlers: the wallet actions behind the
//! [`PaymentHandler`](crate::payments::PaymentHandler) trait.
//!
//! Each rail is feature-gated so a consumer that does not need it pays neither
//! the dependency nor the compile cost.

#[cfg(feature = "nwc")]
pub mod ln_bolt11_nwc;

#[cfg(feature = "nwc")]
pub use ln_bolt11_nwc::{LnBolt11NwcPaymentHandler, LnBolt11NwcPaymentHandlerOptions};
