//! Typed errors for the NIP-47 client.
//!
//! The rail used to return `PaymentError::Processor(String)` for everything,
//! which lost two things a caller needs.
//!
//! **Which side failed.** A client-side wallet timeout surfaced from
//! `LnBolt11NwcPaymentHandler` as `Processor`, not `Handler`. Mapping happens
//! once, at the trait boundary, via [`NwcError::into_processor_error`] and
//! [`NwcError::into_handler_error`].
//!
//! **Whether the wallet may already have acted.** `pay_invoice` is not
//! idempotent. A timeout *after* the request was published may mean the wallet
//! paid and we simply did not hear the answer; a failure *before* the publish
//! means it certainly did not. [`NwcError::Timeout`] carries `published` so a
//! caller can tell those apart instead of guessing, and
//! [`NwcError::may_have_reached_wallet`] answers it directly.
//!
//! Every message here is payload-free: a serde failure reports its category
//! plus line and column, never the offending bytes, because a wallet can put a
//! BOLT11 invoice in a response and both payment middlewares log a
//! `PaymentError` at warn.

use crate::payments::errors::PaymentError;

/// Something that went wrong talking to a NIP-47 wallet.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NwcError {
    /// No answer arrived within the deadline.
    #[error(
        "NWC {method} timed out after {timeout_ms} ms ({})",
        if *published { "request was published; the wallet may have acted on it" }
        else { "request was never published" }
    )]
    Timeout {
        /// The NIP-47 method that timed out.
        method: String,
        /// The deadline that elapsed, in milliseconds.
        timeout_ms: u64,
        /// Whether the request reached a relay before the deadline. When true,
        /// a non-idempotent call such as `pay_invoice` may already have been
        /// executed by the wallet.
        published: bool,
    },

    /// The client was shut down, or is shutting down.
    #[error("NWC client is closed")]
    Closed,

    /// Connecting to the wallet's relays failed.
    #[error("NWC relay connect failed: {reason}")]
    Connect {
        /// Transport-level reason, without any wallet payload.
        reason: String,
    },

    /// Subscribing for responses failed.
    #[error("NWC subscribe failed: {reason}")]
    Subscribe {
        /// Transport-level reason, without any wallet payload.
        reason: String,
    },

    /// Publishing the request failed.
    #[error("NWC publish failed for {method}: {reason}")]
    Publish {
        /// The NIP-47 method whose request could not be published.
        method: String,
        /// Transport-level reason, without any wallet payload.
        reason: String,
    },

    /// Encrypting a request, or decrypting a response, failed.
    #[error("NWC {direction} failed: {reason}")]
    Crypto {
        /// `"encryption"` or `"decryption"`.
        direction: &'static str,
        /// Category only, never the ciphertext or plaintext.
        reason: String,
    },

    /// The response did not parse.
    ///
    /// Deliberately carries no payload: serde's own message quotes the
    /// offending value, and that value can be an invoice.
    #[error("NWC response for {method} could not be parsed ({category} at line {line}, column {column})")]
    MalformedResponse {
        /// The NIP-47 method whose response could not be parsed.
        method: String,
        /// serde's classification (`data`, `syntax`, `eof`, `io`).
        category: &'static str,
        /// Line of the parse failure.
        line: usize,
        /// Column of the parse failure.
        column: usize,
    },

    /// The wallet answered, and said no.
    #[error("NWC {method} failed: {detail}")]
    Wallet {
        /// The NIP-47 method the wallet refused.
        method: String,
        /// Already sanitized by
        /// [`NwcErrorBody::describe`](crate::payments::nip47::NwcErrorBody::describe).
        detail: String,
    },

    /// A request amount was rejected before any wallet call.
    #[error("NWC invalid amount: {reason}")]
    InvalidAmount {
        /// Why the amount was refused.
        reason: String,
    },

    /// A connection string, or an option, was invalid.
    #[error("NWC configuration error: {reason}")]
    Config {
        /// What was wrong.
        reason: String,
    },
}

impl NwcError {
    /// Whether the wallet may already have acted on the request.
    ///
    /// True only when the request reached a relay. The distinction matters for
    /// `pay_invoice`, which is not idempotent: a caller that retries a
    /// `may_have_reached_wallet` failure risks paying twice.
    pub fn may_have_reached_wallet(&self) -> bool {
        match self {
            Self::Timeout { published, .. } => *published,
            // The wallet answered, so it certainly saw the request.
            Self::Wallet { .. } | Self::MalformedResponse { .. } => true,
            Self::Crypto { direction, .. } => *direction == "decryption",
            _ => false,
        }
    }

    /// Map to the crate error a [`PaymentProcessor`](crate::payments::PaymentProcessor) returns.
    pub fn into_processor_error(self) -> PaymentError {
        PaymentError::Processor(self.to_string())
    }

    /// Map to the crate error a [`PaymentHandler`](crate::payments::PaymentHandler) returns.
    pub fn into_handler_error(self) -> PaymentError {
        PaymentError::Handler(self.to_string())
    }

    /// Build a [`Self::MalformedResponse`] from a serde failure without
    /// reproducing any of the offending bytes.
    pub(crate) fn from_serde(method: &str, err: &serde_json::Error) -> Self {
        let category = match err.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Eof => "eof",
        };
        Self::MalformedResponse {
            method: method.to_string(),
            category,
            line: err.line(),
            column: err.column(),
        }
    }
}

impl From<NwcError> for PaymentError {
    /// Defaults to the processor arm. Handler call sites use
    /// [`NwcError::into_handler_error`] explicitly.
    fn from(err: NwcError) -> Self {
        err.into_processor_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_timeout_says_the_wallet_may_have_acted() {
        let e = NwcError::Timeout {
            method: "pay_invoice".into(),
            timeout_ms: 60_000,
            published: true,
        };
        assert!(e.may_have_reached_wallet());
        let text = e.to_string();
        assert!(text.contains("may have acted"), "{text}");
    }

    #[test]
    fn an_unpublished_timeout_says_it_did_not() {
        let e = NwcError::Timeout {
            method: "pay_invoice".into(),
            timeout_ms: 60_000,
            published: false,
        };
        assert!(!e.may_have_reached_wallet());
        assert!(e.to_string().contains("never published"));
    }

    #[test]
    fn maps_to_the_right_side_of_the_trait_boundary() {
        let e = || NwcError::Closed;
        assert!(matches!(
            e().into_processor_error(),
            PaymentError::Processor(_)
        ));
        assert!(matches!(e().into_handler_error(), PaymentError::Handler(_)));
    }

    /// serde's own message quotes the offending value, which can be an invoice.
    #[test]
    fn a_parse_failure_never_reproduces_the_payload() {
        let invoice = "lnbc210n1pjqqqqqpp5abcdefghijklmnopqrstuvwxyz0123456789";
        let raw = format!(r#"{{"result_type":{{"nested":"{invoice}"}}}}"#);
        let err = serde_json::from_str::<serde_json::Value>("{ not json").unwrap_err();
        let mapped = NwcError::from_serde("make_invoice", &err);
        let text = mapped.to_string();
        assert!(!text.contains(invoice), "{text}");
        assert!(!text.contains("lnbc"), "{text}");
        assert!(text.contains("make_invoice") && text.contains("line"));
        // The raw payload is not consulted at all.
        assert!(!raw.is_empty());
    }

    #[test]
    fn config_and_amount_errors_do_not_reach_the_wallet() {
        assert!(!NwcError::InvalidAmount { reason: "x".into() }.may_have_reached_wallet());
        assert!(!NwcError::Config { reason: "x".into() }.may_have_reached_wallet());
        assert!(!NwcError::Closed.may_have_reached_wallet());
    }
}
