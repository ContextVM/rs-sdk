//! Permissive NIP-47 wire types.
//!
//! These deliberately do not reuse `nostr_sdk::nips::nip47`'s response structs.
//! Upstream parses strictly: a wallet that omits an optional field, or adds one,
//! fails deserialization. On a money path that turns a *paid* invoice into a
//! failed verification, so every field here is optional and unknown fields are
//! ignored, field for field with ts `nip47/types.ts`.
//!
//! Requests are still built with the upstream types, where strictness costs
//! nothing because we control the value.

use serde::{Deserialize, Serialize};

use crate::payments::errors::PaymentError;

/// Envelope every NIP-47 response shares: `{result_type, error, result}`.
///
/// `error` and `result` are both nullable per NIP-47, and a wallet may send
/// neither. `result_type` is the only field the client relies on.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NwcResponseEnvelope<R> {
    /// Echo of the request method, e.g. `"make_invoice"`.
    #[serde(default)]
    pub result_type: Option<String>,
    /// Set when the wallet refused or failed the request.
    #[serde(default)]
    pub error: Option<NwcError>,
    /// Set when the wallet succeeded.
    ///
    /// No `#[serde(default)]`: on a generic field that would impose
    /// `R: Default`, and serde already reads a missing `Option` as `None`.
    pub result: Option<R>,
}

/// A wallet-reported error. `code` is an uppercase NIP-47 code such as
/// `NOT_FOUND` or `INSUFFICIENT_BALANCE`; unknown codes are kept verbatim
/// rather than mapped to an enum, so a wallet extension cannot be lost.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NwcError {
    /// Machine-readable error code.
    #[serde(default)]
    pub code: Option<String>,
    /// Human-readable message.
    #[serde(default)]
    pub message: Option<String>,
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
    /// Returns the wallet's [`NwcError`] when `error` is set, and a synthesized
    /// one when the wallet sent neither `error` nor `result`.
    pub fn into_result(self) -> Result<R, NwcError> {
        if let Some(err) = self.error {
            return Err(err);
        }
        self.result.ok_or_else(|| NwcError {
            code: Some("EMPTY_RESPONSE".to_string()),
            message: Some("wallet returned neither result nor error".to_string()),
        })
    }
}

impl NwcError {
    /// `code` uppercased for comparison, or `""` when absent.
    pub fn code_upper(&self) -> String {
        self.code.as_deref().unwrap_or_default().to_uppercase()
    }

    /// A display string usable in a [`PaymentError`] message.
    pub fn describe(&self) -> String {
        match (self.code.as_deref(), self.message.as_deref()) {
            (Some(c), Some(m)) => format!("{c}: {m}"),
            (Some(c), None) => c.to_string(),
            (None, Some(m)) => m.to_string(),
            (None, None) => "unspecified wallet error".to_string(),
        }
    }
}

/// Result of `make_invoice` / `lookup_invoice`.
///
/// Every field optional: wallets differ on which they populate, and a missing
/// `state` with a non-zero `settled_at` still means settled.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NwcInvoiceResult {
    /// `"incoming"` or `"outgoing"`.
    #[serde(default)]
    pub r#type: Option<String>,
    /// `pending` | `settled` | `accepted` | `expired` | `failed`.
    #[serde(default)]
    pub state: Option<String>,
    /// The BOLT11 invoice string.
    #[serde(default)]
    pub invoice: Option<String>,
    /// Payment hash, hex.
    #[serde(default)]
    pub payment_hash: Option<String>,
    /// Preimage, present once settled.
    #[serde(default)]
    pub preimage: Option<String>,
    /// Creation time, unix seconds.
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Expiry time, unix seconds.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Settlement time, unix seconds. Non-zero implies settled.
    #[serde(default)]
    pub settled_at: Option<i64>,
    /// Amount, msats.
    #[serde(default)]
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
pub struct NwcPayInvoiceResult {
    /// Payment preimage, hex.
    #[serde(default)]
    pub preimage: Option<String>,
    /// Routing fees paid, msats.
    #[serde(default)]
    pub fees_paid: Option<i64>,
}

/// A decoded NIP-47 notification payload (`{notification_type, notification}`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NwcNotificationPayload {
    /// e.g. `payment_received`, `payment_sent`.
    pub notification_type: String,
    /// Body, shaped per `notification_type`. Left opaque.
    #[serde(default)]
    pub notification: serde_json::Value,
}

impl NwcNotificationPayload {
    /// `notification.payment_hash`, when present.
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
/// Returns [`PaymentError::Processor`] when `sats` is not positive, or when
/// `sats * 1000` overflows [`i64`].
pub fn sats_to_msats(sats: i64) -> Result<u64, PaymentError> {
    if sats <= 0 {
        return Err(PaymentError::Processor(format!(
            "invoice amount must be positive, got {sats} sats"
        )));
    }
    let msats = sats.checked_mul(1000).ok_or_else(|| {
        PaymentError::Processor(format!(
            "invoice amount overflows when converted to msats: {sats} sats"
        ))
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

    #[test]
    fn envelope_parses_unknown_error_code_verbatim() {
        let raw =
            r#"{"result_type":"pay_invoice","error":{"code":"SOMETHING_NEW","message":"nope"}}"#;
        let env: NwcResponseEnvelope<NwcPayInvoiceResult> = serde_json::from_str(raw).unwrap();
        let err = env.error.unwrap();
        assert_eq!(err.code_upper(), "SOMETHING_NEW");
        assert_eq!(err.describe(), "SOMETHING_NEW: nope");
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
}
