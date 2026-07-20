//! Error taxonomy for CEP-8 payments.
//!
//! Mirrors the sibling per-module error enums
//! (`transport/open_stream/errors.rs`, `transport/oversized_transfer/errors.rs`)
//! so failure classification is consistent across the crate. Surfaced through
//! the crate-level [`crate::Error`] via a `#[from]` conversion on
//! `Error::Payment`.
//!
//! It is `#[non_exhaustive]` because it grows as later CEP-8 work adds variants
//! (canonicalization, payment selection and verification).
//! Unlike the all-`String` siblings it deliberately does not derive
//! `Clone`/`PartialEq`/`Eq`: it carries a [`serde_json::Error`], which is not `Clone`.

/// Errors raised while pricing, issuing, verifying, or paying a CEP-8 invocation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PaymentError {
    /// A server-side [`PaymentProcessor`](crate::payments::PaymentProcessor)
    /// failed to create or verify a payment.
    #[error("Payment processor error: {0}")]
    Processor(String),

    /// A client-side [`PaymentHandler`](crate::payments::PaymentHandler)
    /// failed to execute a payment.
    #[error("Payment handler error: {0}")]
    Handler(String),

    /// (De)serialization of a payment payload failed.
    #[error("Payment serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn folds_into_crate_error_payment_arm() {
        let e: Error = PaymentError::Processor("x".to_string()).into();
        assert!(matches!(e, Error::Payment(_)));
    }

    #[test]
    fn display_nests_through_crate_error() {
        let e: Error = PaymentError::Processor("x".to_string()).into();
        assert_eq!(e.to_string(), "Payment error: Payment processor error: x");
    }

    #[test]
    fn handler_variant_display() {
        assert_eq!(
            PaymentError::Handler("boom".to_string()).to_string(),
            "Payment handler error: boom"
        );
    }

    #[test]
    fn serde_json_error_converts_via_question_mark() {
        fn parse() -> Result<(), PaymentError> {
            let _v: serde_json::Value = serde_json::from_str("{ not json")?;
            Ok(())
        }
        assert!(matches!(
            parse().unwrap_err(),
            PaymentError::Serialization(_)
        ));
    }
}
