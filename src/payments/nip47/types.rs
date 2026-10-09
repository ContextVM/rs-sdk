//! Permissive NIP-47 wire types.
//!
//! These deliberately do not reuse `nostr_sdk::nips::nip47`'s response structs.
//! Upstream parses strictly, and on a money path strictness is the dangerous
//! direction: a wallet that omits an optional field, adds one, or computes one
//! with the wrong JSON type must not turn a *paid* invoice into a failed
//! verification.
//!
//! Two separate leniencies, which are easy to conflate:
//!
//! * **Absent** fields deserialize to `None`. That is `#[serde(default)]`.
//! * **Present but wrongly typed** fields also deserialize to `None`. That is
//!   *not* what `#[serde(default)]` does: serde would fail the whole envelope,
//!   losing a response whose other fields said `"state": "settled"`. Every
//!   optional field below goes through [`lenient`] to get this.
//!
//! Requests are still built with the upstream types, where strictness costs
//! nothing because we control the value.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

/// Upper bound on any wallet-supplied free text reproduced in an error.
const MAX_WALLET_TEXT: usize = 120;

/// Deserialize an optional field, treating a wrong-typed value as absent.
///
/// `#[serde(default)]` alone covers a *missing* key only. A wallet that sends
/// `"settled_at": "2026-01-01T00:00:00Z"` where an integer is expected would
/// otherwise fail the entire response, and the caller would read that as "not
/// settled" for an invoice the payer has already paid.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| serde_json::from_value::<T>(value).ok()))
}

/// Envelope every NIP-47 response shares: `{result_type, error, result}`.
///
/// `error` and `result` are both nullable per NIP-47, and a wallet may send
/// neither.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[non_exhaustive]
pub struct NwcResponseEnvelope<R> {
    /// Echo of the request method, e.g. `"make_invoice"`.
    ///
    /// Not used for correlation: the response is already correlated by the `e`
    /// tag naming the request event, which is strictly stronger. A mismatch is
    /// logged and otherwise tolerated, since rejecting on it would discard a
    /// settlement a wallet labelled oddly.
    #[serde(default, deserialize_with = "lenient")]
    pub result_type: Option<String>,
    /// Set when the wallet refused or failed the request.
    #[serde(default, deserialize_with = "lenient")]
    pub error: Option<NwcErrorBody>,
    /// Set when the wallet succeeded.
    ///
    /// No `#[serde(default)]`: on a generic field that would impose
    /// `R: Default`, and serde already reads a missing `Option` as `None`.
    pub result: Option<R>,
}

impl<R> NwcResponseEnvelope<R> {
    /// Collapse the envelope into a `Result`, so a wallet-reported error is
    /// never mistaken for success.
    ///
    /// Callers that need to classify the error code (the processor treats
    /// `NOT_FOUND` as "still pending" but other codes as fatal) should match on
    /// [`Self::error`] directly instead.
    ///
    /// # Errors
    ///
    /// Returns the wallet's [`NwcErrorBody`] when `error` is set, and a
    /// synthesized one when the wallet sent neither `error` nor `result`.
    pub fn into_result(self) -> Result<R, NwcErrorBody> {
        if let Some(err) = self.error {
            return Err(err);
        }
        self.result.ok_or_else(|| NwcErrorBody {
            code: Some("EMPTY_RESPONSE".to_string()),
            message: Some("wallet returned neither result nor error".to_string()),
        })
    }
}

/// A wallet-reported error. `code` is an uppercase NIP-47 code such as
/// `NOT_FOUND` or `INSUFFICIENT_BALANCE`; unknown codes are kept verbatim
/// rather than mapped to an enum, so a wallet extension cannot be lost.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct NwcErrorBody {
    /// Machine-readable error code.
    #[serde(default, deserialize_with = "lenient")]
    pub code: Option<String>,
    /// Human-readable message, as the wallet sent it.
    ///
    /// Free text under the wallet's control. Never reproduce it raw in a log
    /// or an error: a wallet can put a BOLT11 invoice in here, and both payment
    /// middlewares log a `PaymentError` at warn. Use [`Self::describe`].
    #[serde(default, deserialize_with = "lenient")]
    pub message: Option<String>,
}

impl NwcErrorBody {
    /// `code` uppercased for comparison, or `""` when absent.
    pub fn code_upper(&self) -> String {
        self.code.as_deref().unwrap_or_default().to_uppercase()
    }

    /// A display string safe to put in an error that will be logged.
    ///
    /// The code is reproduced as-is (it is a short enum-like token). The
    /// message is truncated and scrubbed of anything that looks like a BOLT11
    /// invoice, which is a bearer payment request.
    pub fn describe(&self) -> String {
        let code = self.code.as_deref().unwrap_or("UNSPECIFIED");
        match self.message.as_deref().map(sanitize_wallet_text) {
            Some(msg) if !msg.is_empty() => format!("{code}: {msg}"),
            _ => code.to_string(),
        }
    }
}

/// Bound and scrub wallet-controlled free text for inclusion in an error.
///
/// Redacts BOLT11-looking tokens (`lnbc…` and the other network prefixes) and
/// truncates, so neither an invoice nor an unbounded wallet string reaches a
/// log line.
pub(crate) fn sanitize_wallet_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_WALLET_TEXT));
    for word in text.split_whitespace() {
        let lower = word.to_lowercase();
        let looks_like_invoice = ["lnbc", "lntb", "lnbcrt", "lnsb"]
            .iter()
            .any(|p| lower.starts_with(p))
            && word.len() > 20;
        if !out.is_empty() {
            out.push(' ');
        }
        if looks_like_invoice {
            out.push_str("<redacted invoice>");
        } else {
            out.push_str(word);
        }
        if out.len() >= MAX_WALLET_TEXT {
            out.truncate(MAX_WALLET_TEXT);
            out.push_str("...");
            break;
        }
    }
    out
}

/// Result of `make_invoice` / `lookup_invoice`.
///
/// Every field optional and leniently typed: wallets differ on which they
/// populate and on how they encode them, and a missing `state` with a non-zero
/// `settled_at` still means settled.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[non_exhaustive]
pub struct NwcInvoiceResult {
    /// `"incoming"` or `"outgoing"`.
    #[serde(default, deserialize_with = "lenient")]
    pub r#type: Option<String>,
    /// `pending` | `settled` | `accepted` | `expired` | `failed`.
    #[serde(default, deserialize_with = "lenient")]
    pub state: Option<String>,
    /// The BOLT11 invoice string.
    #[serde(default, deserialize_with = "lenient")]
    pub invoice: Option<String>,
    /// Payment hash, hex.
    #[serde(default, deserialize_with = "lenient")]
    pub payment_hash: Option<String>,
    /// Preimage, present once settled.
    #[serde(default, deserialize_with = "lenient")]
    pub preimage: Option<String>,
    /// Creation time, unix seconds.
    #[serde(default, deserialize_with = "lenient")]
    pub created_at: Option<i64>,
    /// Expiry time, unix seconds.
    #[serde(default, deserialize_with = "lenient")]
    pub expires_at: Option<i64>,
    /// Settlement time, unix seconds. Non-zero implies settled.
    #[serde(default, deserialize_with = "lenient")]
    pub settled_at: Option<i64>,
    /// Amount, msats.
    #[serde(default, deserialize_with = "lenient")]
    pub amount: Option<i64>,
}

impl NwcInvoiceResult {
    /// Whether this result represents a settled invoice.
    ///
    /// Two independent signals, either sufficient: an explicit `state ==
    /// "settled"`, or a positive `settled_at`. Wallets populate one or the
    /// other, and requiring both would strand real payments.
    pub fn is_settled(&self) -> bool {
        let by_state = self
            .state
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("settled"));
        let by_timestamp = self.settled_at.is_some_and(|t| t > 0);
        by_state || by_timestamp
    }

    /// Whether this result is terminal-failed (never going to settle).
    pub fn is_terminal_failure(&self) -> bool {
        self.state
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("expired") || s.eq_ignore_ascii_case("failed"))
    }
}

/// Result of `pay_invoice`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[non_exhaustive]
pub struct NwcPayInvoiceResult {
    /// Payment preimage, hex.
    #[serde(default, deserialize_with = "lenient")]
    pub preimage: Option<String>,
    /// Routing fees paid, msats.
    #[serde(default, deserialize_with = "lenient")]
    pub fees_paid: Option<i64>,
}

/// A decoded NIP-47 notification payload (`{notification_type, notification}`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[non_exhaustive]
pub struct NwcNotificationPayload {
    /// e.g. `payment_received`, `payment_sent`.
    pub notification_type: String,
    /// Body, shaped per `notification_type`. Left opaque.
    #[serde(default)]
    pub notification: serde_json::Value,
}

impl NwcNotificationPayload {
    /// `notification.payment_hash`, when present and a string.
    pub fn payment_hash(&self) -> Option<&str> {
        self.notification.get("payment_hash")?.as_str()
    }
}

/// Notification type a wallet emits when an inbound payment settles.
pub const NOTIFICATION_PAYMENT_RECEIVED: &str = "payment_received";

/// Convert sats to msats.
///
/// Hardened against ts, whose `satsToMsats` is a bare multiply: a non-positive
/// amount is rejected rather than turned into a zero-or-negative invoice, and
/// the multiply is checked so a caller-controlled amount cannot wrap. Both are
/// rejected before any wallet call.
///
/// # Errors
///
/// Returns [`crate::payments::nip47::NwcError::InvalidAmount`] when `sats` is
/// not positive, or when `sats * 1000` overflows [`i64`].
pub fn sats_to_msats(sats: i64) -> Result<u64, super::error::NwcError> {
    use super::error::NwcError;
    if sats <= 0 {
        return Err(NwcError::InvalidAmount {
            reason: format!("amount must be positive, got {sats} sats"),
        });
    }
    let msats = sats
        .checked_mul(1000)
        .ok_or_else(|| NwcError::InvalidAmount {
            reason: format!("amount overflows when converted to msats: {sats} sats"),
        })?;
    Ok(msats as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sats_to_msats_converts() {
        assert_eq!(sats_to_msats(1).unwrap(), 1_000);
        assert_eq!(sats_to_msats(21).unwrap(), 21_000);
    }

    #[test]
    fn sats_to_msats_rejects_non_positive() {
        assert!(sats_to_msats(0).is_err());
        assert!(sats_to_msats(-1).is_err());
    }

    #[test]
    fn sats_to_msats_rejects_overflow() {
        assert!(sats_to_msats(i64::MAX).is_err());
        assert!(sats_to_msats(i64::MAX / 999).is_err());
    }

    #[test]
    fn envelope_parses_minimal_settled_result() {
        let raw = r#"{"result_type":"lookup_invoice","result":{"state":"settled"}}"#;
        let env: NwcResponseEnvelope<NwcInvoiceResult> = serde_json::from_str(raw).unwrap();
        assert_eq!(env.result_type.as_deref(), Some("lookup_invoice"));
        assert!(env.error.is_none());
        assert!(env.result.unwrap().is_settled());
    }

    #[test]
    fn envelope_tolerates_unknown_fields_and_nulls() {
        let raw = r#"{"result_type":"make_invoice","error":null,"result":{"invoice":"lnbc1","surprise":42},"extra":true}"#;
        let env: NwcResponseEnvelope<NwcInvoiceResult> = serde_json::from_str(raw).unwrap();
        assert_eq!(env.result.unwrap().invoice.as_deref(), Some("lnbc1"));
    }

    /// The money-path case: one wrongly typed optional field must not discard a
    /// response that says the invoice settled.
    #[test]
    fn off_type_optional_fields_become_none_instead_of_failing_the_response() {
        let raw = r#"{
            "result_type":"lookup_invoice",
            "result":{
                "state":"settled",
                "settled_at":"2026-01-01T00:00:00Z",
                "expires_at":"not-a-number",
                "amount":1234.5,
                "payment_hash":"h"
            }
        }"#;
        let env: NwcResponseEnvelope<NwcInvoiceResult> =
            serde_json::from_str(raw).expect("an off-type field must not fail the envelope");
        let r = env.result.expect("result survives");
        assert!(r.is_settled(), "the settled state must still be read");
        assert_eq!(r.payment_hash.as_deref(), Some("h"));
        assert_eq!(r.settled_at, None, "the off-type value becomes None");
        assert_eq!(r.expires_at, None);
        assert_eq!(r.amount, None);
    }

    #[test]
    fn off_type_state_becomes_none_without_losing_settled_at() {
        let raw =
            r#"{"result_type":"lookup_invoice","result":{"state":7,"settled_at":1700000000}}"#;
        let env: NwcResponseEnvelope<NwcInvoiceResult> = serde_json::from_str(raw).unwrap();
        let r = env.result.unwrap();
        assert_eq!(r.state, None);
        assert!(r.is_settled(), "settled_at still carries the settlement");
    }

    #[test]
    fn off_type_fees_paid_does_not_fail_a_pay_invoice_response() {
        let raw = r#"{"result_type":"pay_invoice","result":{"preimage":"pre","fees_paid":1.5}}"#;
        let env: NwcResponseEnvelope<NwcPayInvoiceResult> = serde_json::from_str(raw).unwrap();
        let r = env.result.unwrap();
        assert_eq!(r.preimage.as_deref(), Some("pre"));
        assert_eq!(r.fees_paid, None);
    }

    #[test]
    fn envelope_parses_unknown_error_code_verbatim() {
        let raw =
            r#"{"result_type":"pay_invoice","error":{"code":"SOMETHING_NEW","message":"nope"}}"#;
        let env: NwcResponseEnvelope<NwcPayInvoiceResult> = serde_json::from_str(raw).unwrap();
        let err = env.error.unwrap();
        assert_eq!(err.code_upper(), "SOMETHING_NEW");
        assert_eq!(err.describe(), "SOMETHING_NEW: nope");
    }

    /// A wallet message can name the invoice, and both middlewares log a
    /// `PaymentError` at warn.
    #[test]
    fn describe_redacts_an_invoice_in_the_wallet_message() {
        let err = NwcErrorBody {
            code: Some("FAILED".into()),
            message: Some(
                "could not pay lnbc210n1pjqqqqqpp5abcdefghijklmnopqrstuvwxyz0123456789 right now"
                    .into(),
            ),
        };
        let described = err.describe();
        assert!(
            !described.contains("lnbc210n1pjqqqqq"),
            "the invoice must not survive into the error: {described}"
        );
        assert!(described.contains("<redacted invoice>"));
        assert!(described.starts_with("FAILED: "));
    }

    #[test]
    fn describe_bounds_an_unbounded_wallet_message() {
        let err = NwcErrorBody {
            code: Some("X".into()),
            message: Some("spam ".repeat(500)),
        };
        assert!(
            err.describe().len() < 200,
            "wallet text must be bounded, got {}",
            err.describe().len()
        );
    }

    #[test]
    fn describe_without_a_message_is_just_the_code() {
        let err = NwcErrorBody {
            code: Some("NOT_FOUND".into()),
            message: None,
        };
        assert_eq!(err.describe(), "NOT_FOUND");
    }

    #[test]
    fn settled_detected_by_timestamp_without_state() {
        let r = NwcInvoiceResult {
            settled_at: Some(1_700_000_000),
            ..Default::default()
        };
        assert!(r.is_settled());
    }

    #[test]
    fn zero_settled_at_is_not_settled() {
        let r = NwcInvoiceResult {
            settled_at: Some(0),
            ..Default::default()
        };
        assert!(!r.is_settled());
    }

    #[test]
    fn pending_is_neither_settled_nor_terminal() {
        let r = NwcInvoiceResult {
            state: Some("pending".into()),
            ..Default::default()
        };
        assert!(!r.is_settled());
        assert!(!r.is_terminal_failure());
    }

    #[test]
    fn expired_and_failed_are_terminal() {
        for s in ["expired", "failed", "EXPIRED"] {
            let r = NwcInvoiceResult {
                state: Some(s.into()),
                ..Default::default()
            };
            assert!(r.is_terminal_failure(), "{s} should be terminal");
            assert!(!r.is_settled());
        }
    }

    #[test]
    fn notification_payload_exposes_payment_hash() {
        let raw =
            r#"{"notification_type":"payment_received","notification":{"payment_hash":"abc"}}"#;
        let n: NwcNotificationPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(n.notification_type, NOTIFICATION_PAYMENT_RECEIVED);
        assert_eq!(n.payment_hash(), Some("abc"));
    }

    #[test]
    fn notification_payload_without_body_is_tolerated() {
        let raw = r#"{"notification_type":"payment_received"}"#;
        let n: NwcNotificationPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(n.payment_hash(), None);
    }

    #[test]
    fn notification_payload_with_off_type_payment_hash_is_none() {
        let raw = r#"{"notification_type":"payment_received","notification":{"payment_hash":42}}"#;
        let n: NwcNotificationPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(n.payment_hash(), None);
    }
}
