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
//! idempotent. A failure after the request reached a relay may mean the wallet
//! paid and we simply did not hear the answer; one before the publish means it
//! certainly did not; and a deadline that fell *inside* the publish means
//! nobody can tell. [`NwcError::Timeout`] and [`NwcError::Closed`] both carry
//! a [`Delivery`] so a caller can distinguish all three instead of guessing,
//! and [`NwcError::may_have_reached_wallet`] answers it directly, treating the
//! unknown case as "may have".
//!
//! Every message here is payload-free: a serde failure reports its category
//! plus line and column, never the offending bytes, because a wallet can put a
//! BOLT11 invoice in a response and both payment middlewares log a
//! `PaymentError` at warn.

use crate::payments::errors::PaymentError;

/// What is known about whether a request reached the wallet.
///
/// Two states are not enough. A deadline that fires *while the publish is
/// still in flight* leaves the answer genuinely unknowable: the pool writes
/// the EVENT before it waits on the relay's OK, so the wallet may well have
/// it. Collapsing that into "not sent" is the dangerous direction, because
/// [`NwcError::may_have_reached_wallet`] is what a caller reads to decide
/// whether retrying a `pay_invoice` is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delivery {
    /// The request was never put on the wire.
    NotSent,
    /// The publish was in flight when the deadline fired. Whether a relay
    /// accepted the event cannot be known from here, so it is treated as
    /// though it may have been.
    Unknown,
    /// A relay accepted the event.
    Sent,
}

impl Delivery {
    /// Whether the wallet may already have acted on the request.
    ///
    /// True for both [`Self::Sent`] and [`Self::Unknown`]: only a request we
    /// know never left is safe to retry blindly.
    pub fn may_have_reached_wallet(self) -> bool {
        matches!(self, Self::Sent | Self::Unknown)
    }

    fn describe(self) -> &'static str {
        match self {
            Self::NotSent => "request was never published",
            Self::Unknown => "request was still being published; the wallet may have acted on it",
            Self::Sent => "request was published; the wallet may have acted on it",
        }
    }
}

/// Something that went wrong talking to a NIP-47 wallet.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NwcError {
    /// No answer arrived within the deadline.
    #[error("NWC {method} timed out after {timeout_ms} ms ({})", delivery.describe())]
    Timeout {
        /// The NIP-47 method that timed out.
        method: String,
        /// The deadline that elapsed, in milliseconds.
        timeout_ms: u64,
        /// What is known about the request reaching a relay.
        delivery: Delivery,
    },

    /// The client was shut down, or is shutting down.
    ///
    /// Carries [`Delivery`] because a `close()` can land *after* a
    /// `pay_invoice` has gone out: the caller still needs to know the wallet
    /// may act on it.
    #[error("NWC client is closed ({})", delivery.describe())]
    Closed {
        /// What is known about the request reaching a relay.
        delivery: Delivery,
    },

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
            Self::Timeout { delivery, .. } | Self::Closed { delivery } => {
                delivery.may_have_reached_wallet()
            }
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
            delivery: Delivery::Sent,
        };
        assert!(e.may_have_reached_wallet());
        let text = e.to_string();
        assert!(text.contains("may have acted"), "{text}");
    }

    /// (8a). The deadline falling inside the publish is the one case that is
    /// genuinely unknowable, and it must not be reported as a definite
    /// non-delivery: the pool writes the EVENT before it waits for the OK.
    #[test]
    fn a_timeout_during_the_publish_is_unknown_not_a_non_delivery() {
        let e = NwcError::Timeout {
            method: "pay_invoice".into(),
            timeout_ms: 60_000,
            delivery: Delivery::Unknown,
        };
        assert!(
            e.may_have_reached_wallet(),
            "an in-flight publish is not a safe retry"
        );
        let text = e.to_string();
        assert!(text.contains("may have acted"), "{text}");
        assert!(!text.contains("never published"), "{text}");
    }

    /// (24). A `close()` that lands after the request went out must keep the
    /// delivery bit, or the caller reads it as a safe retry.
    #[test]
    fn a_close_after_the_publish_still_warns_the_caller() {
        let e = NwcError::Closed {
            delivery: Delivery::Sent,
        };
        assert!(e.may_have_reached_wallet());
        assert!(e.to_string().contains("may have acted"), "{e}");
    }

    #[test]
    fn an_unpublished_timeout_says_it_did_not() {
        let e = NwcError::Timeout {
            method: "pay_invoice".into(),
            timeout_ms: 60_000,
            delivery: Delivery::NotSent,
        };
        assert!(!e.may_have_reached_wallet());
        assert!(e.to_string().contains("never published"));
    }

    #[test]
    fn maps_to_the_right_side_of_the_trait_boundary() {
        let e = || NwcError::Closed {
            delivery: Delivery::NotSent,
        };
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

    /// The previous version built its error from `"{ not json"`, whose serde
    /// message contains no payload, so anything that leaked one still passed.
    /// This one makes serde quote a real invoice and then checks it is gone.
    #[test]
    fn a_parse_failure_on_wallet_json_never_reproduces_the_invoice() {
        let invoice = "lnbc210n1pjqqqqqpp5abcdefghijklmnopqrstuvwxyz0123456789";
        let raw = format!(r#"{{"amount":"{invoice}"}}"#);

        #[derive(Debug, serde::Deserialize)]
        struct Body {
            #[allow(dead_code)]
            amount: u64,
        }
        let err = serde_json::from_str::<Body>(&raw).expect_err("a wrong type");
        assert!(
            err.to_string().contains("lnbc"),
            "precondition: serde is expected to quote the value, got {err}"
        );

        let text = NwcError::from_serde("pay_invoice", &err).to_string();
        assert!(!text.contains(invoice), "{text}");
        assert!(!text.contains("lnbc"), "{text}");
        assert!(
            text.contains("pay_invoice") && text.contains("line"),
            "{text}"
        );
    }

    #[test]
    fn config_and_amount_errors_do_not_reach_the_wallet() {
        assert!(!NwcError::InvalidAmount { reason: "x".into() }.may_have_reached_wallet());
        assert!(!NwcError::Config { reason: "x".into() }.may_have_reached_wallet());
        assert!(!NwcError::Closed {
            delivery: Delivery::NotSent
        }
        .may_have_reached_wallet());
    }
}
