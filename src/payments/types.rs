//! CEP-8 payment types: wire notification params, explicit-gating error `data`,
//! server-side pricing config, and the trait param / return structs.
//!
//! Wire structs derive `Serialize`/`Deserialize`; every optional carries
//! `#[serde(default, skip_serializing_if = "Option::is_none")]` so a `None`
//! renders byte-identically to TS dropping an `undefined` key. `_meta` is a
//! permissive object and no wire type uses `deny_unknown_fields` (CEP-8 MUST:
//! unknown `_meta`/params keys are ignored).
//!
//! **Struct field order is load-bearing.** serde emits keys in declaration
//! order and these payloads are plain JSON (not JCS-canonicalized), so each
//! struct matches the ts-sdk runtime emitter's object-construction order. Note
//! the deliberate difference between [`PaymentRequiredParams`] (`amount`,
//! **`pay_req`, `pmi`**) and [`PaymentOption`] (`amount`, **`pmi`, `pay_req`**);
//! both mirror their respective ts emitters exactly.

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::core::types::JsonRpcRequest;

/// Permissive transparency metadata (`_meta`), a JSON object of arbitrary keys.
///
/// This is `serde_json::Map`. The crate does not enable `serde_json/preserve_order`,
/// so multi-key `_meta` serializes in **sorted** key order (a `BTreeMap`), which
/// can differ from a JS peer's insertion order. This is cosmetic only: `_meta` is
/// opaque pass-through and never enters a cross-checked hash (the canonical
/// invocation hash strips top-level `params._meta` and JCS re-sorts regardless;
/// notifications are never canonicalized). If strict multi-key `_meta` byte-parity
/// with ts is ever required, enable `serde_json/preserve_order`.
pub type Meta = serde_json::Map<String, serde_json::Value>;

// ── Config / pricing ────────────────────────────────────────────────

/// Server-side pricing metadata for one capability pattern (config, not a wire type).
///
/// The `cap` tag is this struct's wire form (see [`crate::payments::tags`]), so
/// it carries no serde/field-order contract.
#[derive(Debug, Clone)]
pub struct PricedCapability {
    /// JSON-RPC method, e.g. `"tools/call"`. Unknown methods yield no `cap` tag.
    pub method: String,
    /// Capability name (tool/prompt name, resource uri). `None` yields no `cap` tag.
    pub name: Option<String>,
    /// Amount for this invocation.
    pub amount: i64,
    /// Optional upper bound; when set the `cap` price is `"<amount>-<max_amount>"`.
    pub max_amount: Option<i64>,
    /// Currency/unit label for display and the `cap` tag (e.g. `"sats"`).
    pub currency_unit: String,
    /// Optional human-readable description for the payment request.
    pub description: Option<String>,
}

/// Server policy for which payment lifecycles it accepts (config; mirrors the OPTIONAL enc/giftwrap pattern).
///
/// Wire values are `"optional"` / `"transparent"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentInteractionPolicy {
    /// Accept both lifecycles; mirror the client's requested mode (default).
    #[default]
    Optional,
    /// Transparent-only; reject `explicit_gating` with `-32602`.
    Transparent,
}

// ── Transparent-lifecycle notification params ───────────────────────

/// `notifications/payment_required` params. Emitter key order: amount, pay_req, pmi, description, ttl, _meta.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequiredParams {
    /// Amount to pay, in the capability's currency unit.
    pub amount: i64,
    /// Opaque payment request (e.g. a BOLT11 invoice) the client settles.
    pub pay_req: String,
    /// Payment method identifier this request is denominated in.
    pub pmi: String,
    /// Optional human-readable description of the charge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional time-to-live in seconds before the request expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
    /// Optional transparency metadata (`_meta`), passed through opaquely.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// `notifications/payment_accepted` params. Emitter key order: amount, pmi, _meta.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentAcceptedParams {
    /// Amount that was settled.
    pub amount: i64,
    /// Payment method identifier that settled.
    pub pmi: String,
    /// Optional settlement metadata (`_meta`) from `verify_payment`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// `notifications/payment_rejected` params. Emitter key order: pmi, amount, message. (No `_meta` in TS.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRejectedParams {
    /// Payment method identifier that was rejected.
    pub pmi: String,
    /// Optional amount that was rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<i64>,
    /// Optional human-readable rejection reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ── Explicit-gating error `data` ────────────────────────────────────

/// One entry in a `-32042` `payment_options`. Emitter key order: amount, pmi, pay_req, description, ttl, _meta.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentOption {
    /// Amount to pay for this option.
    pub amount: i64,
    /// Payment method identifier for this option.
    pub pmi: String,
    /// Opaque payment request the client settles for this option.
    pub pay_req: String,
    /// Optional human-readable description of this option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional time-to-live in seconds for this option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
    /// Optional transparency metadata (`_meta`) for this option.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// `error.data` for `-32042 Payment Required`. Emitter key order: instructions, payment_options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequiredErrorData {
    /// Optional human-readable payment instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// One or more concrete payment options (at least one per CEP-8).
    pub payment_options: Vec<PaymentOption>,
}

/// `error.data` for `-32043 Payment Pending`. Emitter key order: instructions, retry_after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentPendingErrorData {
    /// Optional human-readable pending instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Optional seconds the client SHOULD wait before retrying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
}

// ── Trait param / return structs (internal; not wire types) ─────────

/// Input to [`PaymentProcessor::create_payment_required`](crate::payments::PaymentProcessor::create_payment_required).
#[derive(Debug, Clone)]
pub struct PaymentProcessorCreateParams {
    /// Amount to charge for this invocation.
    pub amount: i64,
    /// Optional description to carry into `payment_required`.
    pub description: Option<String>,
    /// Correlating Nostr event id of the gated request.
    pub request_event_id: String,
    /// Caller (client) pubkey, hex.
    pub client_pubkey: String,
}

/// Input to [`PaymentProcessor::verify_payment`](crate::payments::PaymentProcessor::verify_payment).
/// `cancel` is a per-verify child token (timeout / shutdown).
#[derive(Debug, Clone)]
pub struct PaymentProcessorVerifyParams {
    /// The `pay_req` previously issued for this invocation.
    pub pay_req: String,
    /// Correlating Nostr event id of the gated request.
    pub request_event_id: String,
    /// Caller (client) pubkey, hex.
    pub client_pubkey: String,
    /// Per-verify cancellation (timeout / shutdown); a child of the transport token.
    pub cancel: CancellationToken,
}

/// Outcome of [`PaymentProcessor::verify_payment`](crate::payments::PaymentProcessor::verify_payment);
/// `meta` flows into `payment_accepted._meta`.
#[derive(Debug, Clone, Default)]
pub struct VerifyOutcome {
    /// Optional settlement metadata attached to `payment_accepted`.
    pub meta: Option<Meta>,
}

/// Input to [`PaymentHandler`](crate::payments::PaymentHandler) (client-side wallet action).
/// Mirrors `payment_required` plus the event id.
#[derive(Debug, Clone)]
pub struct PaymentHandlerRequest {
    /// Amount to pay.
    pub amount: i64,
    /// Opaque payment request to settle.
    pub pay_req: String,
    /// Payment method identifier.
    pub pmi: String,
    /// Optional description from the server.
    pub description: Option<String>,
    /// Optional time-to-live in seconds from the server.
    pub ttl: Option<u64>,
    /// Optional transparency metadata from the server's `payment_required._meta`.
    pub meta: Option<Meta>,
    /// Correlating Nostr event id of the request being paid.
    pub request_event_id: String,
}

/// Input to [`ResolvePrice::resolve_price`](crate::payments::ResolvePrice::resolve_price).
#[derive(Debug, Clone)]
pub struct ResolvePriceParams {
    /// The matched priced-capability config.
    pub capability: PricedCapability,
    /// The full inbound request being priced.
    pub request: JsonRpcRequest,
    /// Caller (client) pubkey, hex.
    pub client_pubkey: String,
    /// Correlating Nostr event id.
    pub request_event_id: String,
}

/// The dynamic-pricing decision. Modeling this as an enum (not a `reject: true` /
/// `waive: true` union) removes the TS discriminant-typo footgun that
/// `rejectPrice` / `waivePrice` guard against. Never (de)serialized (internal
/// callback return).
#[derive(Debug, Clone)]
pub enum ResolvePriceResult {
    /// Charge this invocation. `meta` rides `payment_required.params._meta`.
    Quote {
        /// Final amount to charge.
        amount: i64,
        /// Optional description override.
        description: Option<String>,
        /// Optional transparency metadata.
        meta: Option<Meta>,
    },
    /// Reject without asking for payment (emits `payment_rejected` / `-32000`).
    Reject {
        /// Optional human-readable rejection reason.
        message: Option<String>,
    },
    /// Waive/cover; forward immediately. `meta` rides `payment_accepted._meta` if emitted.
    Waive {
        /// Optional transparency metadata (e.g. remaining balance).
        meta: Option<Meta>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_key_meta() -> Meta {
        let mut m = Meta::new();
        m.insert("k".to_string(), serde_json::Value::String("v".to_string()));
        m
    }

    fn to_json<T: Serialize>(v: &T) -> String {
        serde_json::to_string(v).unwrap()
    }

    // ── PaymentRequiredParams: amount, pay_req, pmi, description, ttl, _meta ──

    #[test]
    fn payment_required_params_drops_none_optionals() {
        let p = PaymentRequiredParams {
            amount: 1,
            pay_req: "inv".to_string(),
            pmi: "bitcoin-lightning-bolt11".to_string(),
            description: None,
            ttl: None,
            meta: None,
        };
        assert_eq!(
            to_json(&p),
            r#"{"amount":1,"pay_req":"inv","pmi":"bitcoin-lightning-bolt11"}"#
        );
    }

    #[test]
    fn payment_required_params_full_shape_and_field_order() {
        let p = PaymentRequiredParams {
            amount: 1,
            pay_req: "inv".to_string(),
            pmi: "x".to_string(),
            description: Some("d".to_string()),
            ttl: Some(600),
            meta: Some(one_key_meta()),
        };
        assert_eq!(
            to_json(&p),
            r#"{"amount":1,"pay_req":"inv","pmi":"x","description":"d","ttl":600,"_meta":{"k":"v"}}"#
        );
    }

    #[test]
    fn payment_required_params_roundtrips_and_ignores_unknown_keys() {
        let json = r#"{"amount":7,"pay_req":"r","pmi":"m","ttl":1,"unknown":"ignored"}"#;
        let p: PaymentRequiredParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.amount, 7);
        assert_eq!(p.pay_req, "r");
        assert_eq!(p.pmi, "m");
        assert_eq!(p.ttl, Some(1));
        assert!(p.description.is_none());
        let back: PaymentRequiredParams = serde_json::from_str(&to_json(&p)).unwrap();
        assert_eq!(p, back);
    }

    // ── PaymentAcceptedParams: amount, pmi, _meta ───────────────────

    #[test]
    fn payment_accepted_params_shapes() {
        let none = PaymentAcceptedParams {
            amount: 5,
            pmi: "x".to_string(),
            meta: None,
        };
        assert_eq!(to_json(&none), r#"{"amount":5,"pmi":"x"}"#);

        let mut meta = Meta::new();
        meta.insert("settled".to_string(), serde_json::Value::Bool(true));
        let full = PaymentAcceptedParams {
            amount: 5,
            pmi: "x".to_string(),
            meta: Some(meta),
        };
        assert_eq!(
            to_json(&full),
            r#"{"amount":5,"pmi":"x","_meta":{"settled":true}}"#
        );
    }

    // ── PaymentRejectedParams: pmi, amount, message ─────────────────

    #[test]
    fn payment_rejected_params_shapes_and_order() {
        let min = PaymentRejectedParams {
            pmi: "x".to_string(),
            amount: None,
            message: None,
        };
        assert_eq!(to_json(&min), r#"{"pmi":"x"}"#);

        let full = PaymentRejectedParams {
            pmi: "x".to_string(),
            amount: Some(5),
            message: Some("nope".to_string()),
        };
        assert_eq!(to_json(&full), r#"{"pmi":"x","amount":5,"message":"nope"}"#);
    }

    // ── PaymentOption: amount, pmi, pay_req (the deliberate flip) ────

    #[test]
    fn payment_option_field_order_flips_pmi_before_pay_req() {
        let min = PaymentOption {
            amount: 1,
            pmi: "x".to_string(),
            pay_req: "r".to_string(),
            description: None,
            ttl: None,
            meta: None,
        };
        assert_eq!(to_json(&min), r#"{"amount":1,"pmi":"x","pay_req":"r"}"#);

        let full = PaymentOption {
            amount: 1,
            pmi: "x".to_string(),
            pay_req: "r".to_string(),
            description: Some("d".to_string()),
            ttl: Some(600),
            meta: Some(one_key_meta()),
        };
        assert_eq!(
            to_json(&full),
            r#"{"amount":1,"pmi":"x","pay_req":"r","description":"d","ttl":600,"_meta":{"k":"v"}}"#
        );
    }

    // ── error data ──────────────────────────────────────────────────

    #[test]
    fn payment_required_error_data_shapes() {
        let opt = PaymentOption {
            amount: 1,
            pmi: "x".to_string(),
            pay_req: "r".to_string(),
            description: None,
            ttl: None,
            meta: None,
        };
        let no_instructions = PaymentRequiredErrorData {
            instructions: None,
            payment_options: vec![opt.clone()],
        };
        assert_eq!(
            to_json(&no_instructions),
            r#"{"payment_options":[{"amount":1,"pmi":"x","pay_req":"r"}]}"#
        );

        let with_instructions = PaymentRequiredErrorData {
            instructions: Some("pay".to_string()),
            payment_options: vec![opt],
        };
        assert_eq!(
            to_json(&with_instructions),
            r#"{"instructions":"pay","payment_options":[{"amount":1,"pmi":"x","pay_req":"r"}]}"#
        );
    }

    #[test]
    fn payment_pending_error_data_shapes() {
        let empty = PaymentPendingErrorData {
            instructions: None,
            retry_after: None,
        };
        assert_eq!(to_json(&empty), r#"{}"#);

        let full = PaymentPendingErrorData {
            instructions: Some("wait".to_string()),
            retry_after: Some(2),
        };
        assert_eq!(to_json(&full), r#"{"instructions":"wait","retry_after":2}"#);
    }

    // ── PaymentInteractionPolicy: "optional" / "transparent" ────────

    #[test]
    fn payment_interaction_policy_roundtrips() {
        assert_eq!(
            to_json(&PaymentInteractionPolicy::Optional),
            r#""optional""#
        );
        assert_eq!(
            to_json(&PaymentInteractionPolicy::Transparent),
            r#""transparent""#
        );
        assert_eq!(
            PaymentInteractionPolicy::default(),
            PaymentInteractionPolicy::Optional
        );
        let parsed: PaymentInteractionPolicy = serde_json::from_str(r#""transparent""#).unwrap();
        assert_eq!(parsed, PaymentInteractionPolicy::Transparent);
    }

    // ── u64 fail-closed reject tests (deliberate ts divergence) ──
    //
    // ts types `ttl`/`retry_after` as `number`, so a peer MAY send `-1` or
    // `1.5`; rs `u64` rejects both at deserialize. The client-inbound path
    // must handle this `Err` rather than unwrap. `amount` (i64) shares the
    // fail-closed property for fractional values.

    #[test]
    fn ttl_negative_fails_to_deserialize() {
        let json = r#"{"amount":1,"pay_req":"r","pmi":"x","ttl":-1}"#;
        assert!(serde_json::from_str::<PaymentRequiredParams>(json).is_err());
    }

    #[test]
    fn ttl_fractional_fails_to_deserialize() {
        let json = r#"{"amount":1,"pay_req":"r","pmi":"x","ttl":1.5}"#;
        assert!(serde_json::from_str::<PaymentRequiredParams>(json).is_err());
    }

    #[test]
    fn retry_after_negative_fails_to_deserialize() {
        assert!(serde_json::from_str::<PaymentPendingErrorData>(r#"{"retry_after":-1}"#).is_err());
    }

    #[test]
    fn retry_after_fractional_fails_to_deserialize() {
        assert!(serde_json::from_str::<PaymentPendingErrorData>(r#"{"retry_after":1.5}"#).is_err());
    }

    #[test]
    fn amount_fractional_fails_to_deserialize() {
        // `amount` is i64, so a fractional amount fails closed at
        // deserialize just like a fractional `ttl`. (A negative amount is a
        // valid i64 and is intentionally NOT rejected.)
        let json = r#"{"amount":1.5,"pay_req":"r","pmi":"x"}"#;
        assert!(serde_json::from_str::<PaymentRequiredParams>(json).is_err());
    }

    // ── _meta ordering (pinned; see the `Meta` doc) ─────────────────

    #[test]
    fn multi_key_meta_serializes_in_sorted_order() {
        // `preserve_order` is off, so `Meta` is a BTreeMap: keys emit sorted,
        // not in insertion order. This is cosmetic (never enters a hash) but is
        // pinned so a future change to insertion order is a conscious decision.
        let mut meta = Meta::new();
        meta.insert("zeta".to_string(), serde_json::Value::Bool(true));
        meta.insert("alpha".to_string(), serde_json::Value::from(1));
        let p = PaymentAcceptedParams {
            amount: 5,
            pmi: "x".to_string(),
            meta: Some(meta),
        };
        assert_eq!(
            to_json(&p),
            r#"{"amount":5,"pmi":"x","_meta":{"alpha":1,"zeta":true}}"#
        );
    }

    // ── ResolvePriceResult (never on the wire; smoke-test the variants) ──

    #[test]
    fn resolve_price_result_variants_construct_and_match() {
        let quote = ResolvePriceResult::Quote {
            amount: 42,
            description: Some("d".to_string()),
            meta: None,
        };
        let reject = ResolvePriceResult::Reject {
            message: Some("no".to_string()),
        };
        let waive = ResolvePriceResult::Waive { meta: None };
        assert!(matches!(
            quote,
            ResolvePriceResult::Quote { amount: 42, .. }
        ));
        assert!(matches!(reject, ResolvePriceResult::Reject { .. }));
        assert!(matches!(waive, ResolvePriceResult::Waive { meta: None }));
    }
}
