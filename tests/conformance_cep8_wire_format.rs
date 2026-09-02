//! Conformance tests for the CEP-8 wire format: the `pmi`, `payment_interaction`, and `cap` tag
//! tuples, the `-32602` unsupported-mode negotiation error, and the payment notification params.
//!
//! These pin exact bytes rather than behavior, so a refactor of the builders cannot silently drift
//! the wire format away from the spec and the ts-sdk. Everything here is pure (no relays, no
//! transport), so the file also runs under `--no-default-features`.

use contextvm_sdk::core::constants::tags;
use contextvm_sdk::core::types::{JsonRpcError, PaymentInteractionMode};
use contextvm_sdk::payments::constants::UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE;
use contextvm_sdk::payments::tags::{
    cap_tags_from_priced_capabilities, payment_interaction_tag, pmi_tag, pmi_tags,
};
use contextvm_sdk::{
    PaymentAcceptedParams, PaymentOption, PaymentRejectedParams, PaymentRequiredErrorData,
    PaymentRequiredParams, PricedCapability, UnsupportedPaymentInteractionData,
};
use nostr_sdk::prelude::*;

/// A tag's wire form: the flat string array a relay stores and a peer reads.
fn wire(tag: &Tag) -> Vec<String> {
    tag.clone().to_vec()
}

fn wire_all(list: &[Tag]) -> Vec<Vec<String>> {
    list.iter().map(wire).collect()
}

fn priced(
    method: &str,
    name: &str,
    amount: i64,
    max_amount: Option<i64>,
    unit: &str,
) -> PricedCapability {
    PricedCapability {
        method: method.to_string(),
        name: Some(name.to_string()),
        amount,
        max_amount,
        currency_unit: unit.to_string(),
        description: None,
    }
}

// ── `pmi` tags ───────────────────────────────────────────────────────────────

#[test]
fn cep8_pmi_tag_is_a_two_element_tuple() {
    assert_eq!(
        wire(&pmi_tag("bitcoin-lightning-bolt11")),
        vec!["pmi", "bitcoin-lightning-bolt11"]
    );
    assert_eq!(wire(&pmi_tag("bitcoin-lightning-bolt11"))[0], tags::PMI);
}

#[test]
fn cep8_pmi_tags_emit_one_tag_per_method_in_preference_order() {
    // CEP-8 sends one 2-element `pmi` tag per payment method, in preference order, never a single
    // multi-value tag.
    let list = pmi_tags(&["bitcoin-lightning-bolt11".to_string(), "cashu".to_string()]);
    assert_eq!(
        wire_all(&list),
        vec![
            vec!["pmi", "bitcoin-lightning-bolt11"],
            vec!["pmi", "cashu"],
        ]
    );
}

#[test]
fn cep8_pmi_tags_empty_input_emits_nothing() {
    assert!(pmi_tags(&[]).is_empty());
}

// ── `payment_interaction` tags ───────────────────────────────────────────────

#[test]
fn cep8_payment_interaction_tag_values() {
    assert_eq!(
        wire(&payment_interaction_tag(
            PaymentInteractionMode::ExplicitGating
        )),
        vec!["payment_interaction", "explicit_gating"]
    );
    assert_eq!(
        wire(&payment_interaction_tag(
            PaymentInteractionMode::Transparent
        )),
        vec!["payment_interaction", "transparent"]
    );
    assert_eq!(
        wire(&payment_interaction_tag(
            PaymentInteractionMode::Transparent
        ))[0],
        tags::PAYMENT_INTERACTION
    );
}

// ── `cap` tags ───────────────────────────────────────────────────────────────

#[test]
fn cep8_cap_tag_matches_the_spec_example() {
    let list = cap_tags_from_priced_capabilities(&[priced(
        "tools/call",
        "get_weather",
        100,
        None,
        "sats",
    )]);
    assert_eq!(
        wire_all(&list),
        vec![vec!["cap", "tool:get_weather", "100", "sats"]]
    );
    assert_eq!(wire(&list[0])[0], tags::CAPABILITY);
}

#[test]
fn cep8_cap_tag_identifier_prefixes_by_method() {
    let list = cap_tags_from_priced_capabilities(&[
        priced("tools/call", "add", 10, None, "sats"),
        priced("prompts/get", "summarize", 20, None, "sats"),
        priced("resources/read", "file:///a.txt", 30, None, "sats"),
    ]);
    assert_eq!(
        wire_all(&list),
        vec![
            vec!["cap", "tool:add", "10", "sats"],
            vec!["cap", "prompt:summarize", "20", "sats"],
            vec!["cap", "resource:file:///a.txt", "30", "sats"],
        ]
    );
}

#[test]
fn cep8_cap_tag_range_price_is_hyphenated() {
    let list = cap_tags_from_priced_capabilities(&[priced(
        "tools/call",
        "generate",
        100,
        Some(500),
        "sats",
    )]);
    assert_eq!(
        wire_all(&list),
        vec![vec!["cap", "tool:generate", "100-500", "sats"]]
    );
}

// ── `-32602` unsupported payment_interaction ─────────────────────────────────

#[test]
fn cep8_unsupported_payment_interaction_data_shape() {
    let data = UnsupportedPaymentInteractionData::new(PaymentInteractionMode::ExplicitGating);
    assert_eq!(
        serde_json::to_value(&data).expect("data serializes"),
        serde_json::json!({
            "requested": "explicit_gating",
            "supported": ["transparent"],
        })
    );
}

#[test]
fn cep8_unsupported_payment_interaction_error_object() {
    let error = JsonRpcError {
        code: UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE,
        message: "Unsupported payment_interaction mode: explicit_gating".to_string(),
        data: serde_json::to_value(UnsupportedPaymentInteractionData::new(
            PaymentInteractionMode::ExplicitGating,
        ))
        .ok(),
    };
    assert_eq!(
        serde_json::to_value(&error).expect("error serializes"),
        serde_json::json!({
            "code": -32602,
            "message": "Unsupported payment_interaction mode: explicit_gating",
            "data": {
                "requested": "explicit_gating",
                "supported": ["transparent"],
            },
        })
    );
}

#[test]
fn cep8_unsupported_payment_interaction_code_is_invalid_params() {
    // A negotiation failure is a JSON-RPC invalid-params error, not one of the payment error codes.
    assert_eq!(UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE, -32602);
}

// ── Payment notification params ──────────────────────────────────────────────

#[test]
fn cep8_payment_required_params_shape() {
    let params = PaymentRequiredParams {
        amount: 100,
        pay_req: "lnbc1...".to_string(),
        pmi: "bitcoin-lightning-bolt11".to_string(),
        description: None,
        ttl: None,
        meta: None,
    };
    // Absent optionals are dropped, matching a ts emitter leaving them `undefined`.
    assert_eq!(
        serde_json::to_value(&params).expect("params serialize"),
        serde_json::json!({
            "amount": 100,
            "pay_req": "lnbc1...",
            "pmi": "bitcoin-lightning-bolt11",
        })
    );
}

#[test]
fn cep8_payment_accepted_params_shape() {
    let params = PaymentAcceptedParams {
        amount: 100,
        pmi: "bitcoin-lightning-bolt11".to_string(),
        meta: None,
    };
    assert_eq!(
        serde_json::to_value(&params).expect("params serialize"),
        serde_json::json!({
            "amount": 100,
            "pmi": "bitcoin-lightning-bolt11",
        })
    );
}

#[test]
fn cep8_payment_rejected_params_shape() {
    let params = PaymentRejectedParams {
        pmi: "bitcoin-lightning-bolt11".to_string(),
        amount: Some(100),
        message: Some("insufficient".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&params).expect("params serialize"),
        serde_json::json!({
            "pmi": "bitcoin-lightning-bolt11",
            "amount": 100,
            "message": "insufficient",
        })
    );

    let minimal = PaymentRejectedParams {
        pmi: "cashu".to_string(),
        amount: None,
        message: None,
    };
    assert_eq!(
        serde_json::to_value(&minimal).expect("params serialize"),
        serde_json::json!({ "pmi": "cashu" })
    );
}

// ── explicit-gating full error objects ──────────────────────────────────────
//
// These call the SAME builders the explicit-gating middleware publishes through, so a
// drift in the middleware's wire bytes fails here, not just in its own unit tests. The
// JSON-RPC `id` on all three errors is the original inner request id (the spec's
// examples show the request's own id; the targeted publish path does not rewrite ids).

#[test]
fn cep8_payment_required_error_object() {
    use contextvm_sdk::payments::server_explicit_gating::build_payment_required_error;

    let full = build_payment_required_error(
        serde_json::json!(3),
        PaymentOption {
            amount: 100,
            pmi: "bitcoin-lightning-bolt11".to_string(),
            pay_req: "lnbc1...".to_string(),
            description: Some("Echo tool invocation".to_string()),
            ttl: Some(600),
            meta: None,
        },
    );
    assert_eq!(
        serde_json::to_value(&full).expect("error object serializes"),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": {
                "code": -32042,
                "message": "Payment Required",
                "data": {
                    "instructions": "Payment is required to process this request. Pay one \
                                     of the offered options, then retry the same request \
                                     with exactly the same method and params.",
                    "payment_options": [{
                        "amount": 100,
                        "pmi": "bitcoin-lightning-bolt11",
                        "pay_req": "lnbc1...",
                        "description": "Echo tool invocation",
                        "ttl": 600
                    }]
                }
            }
        })
    );

    // Absent optional fields are omitted from the option object entirely, matching the
    // reference implementation dropping `undefined` values.
    let minimal = build_payment_required_error(
        serde_json::json!("req-1"),
        PaymentOption {
            amount: 21,
            pmi: "cashu".to_string(),
            pay_req: "creq...".to_string(),
            description: None,
            ttl: None,
            meta: None,
        },
    );
    let value = serde_json::to_value(&minimal).expect("error object serializes");
    assert_eq!(
        value["error"]["data"]["payment_options"],
        serde_json::json!([{ "amount": 21, "pmi": "cashu", "pay_req": "creq..." }])
    );
}

#[test]
fn cep8_payment_pending_error_object() {
    use contextvm_sdk::payments::server_explicit_gating::build_payment_pending_error;

    let pending = build_payment_pending_error(serde_json::json!(3), 500);
    assert_eq!(
        serde_json::to_value(&pending).expect("error object serializes"),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": {
                "code": -32043,
                "message": "Payment Pending",
                "data": {
                    "instructions": "A payment is already pending for this invocation. \
                                     Retry the same request later with exactly the same \
                                     method and params.",
                    "retry_after": 1
                }
            }
        })
    );
}

#[test]
fn cep8_payment_rejected_error_object_has_no_data_key() {
    use contextvm_sdk::payments::server_explicit_gating::build_payment_rejected_error;

    let rejected =
        build_payment_rejected_error(serde_json::json!(3), Some("not today".to_string()));
    let value = serde_json::to_value(&rejected).expect("error object serializes");
    assert_eq!(
        value,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32000, "message": "not today" }
        })
    );
    assert!(
        value["error"].get("data").is_none(),
        "the rejection error must carry NO data key at all"
    );

    // A reasonless rejection falls back to the policy message.
    let fallback = build_payment_rejected_error(serde_json::json!(4), None);
    assert_eq!(fallback.error.message, "Payment rejected by policy");
}

#[test]
fn cep8_payment_required_error_always_carries_exactly_one_option() {
    use contextvm_sdk::payments::server_explicit_gating::build_payment_required_error;

    // An empty `payment_options` is representable at the TYPE level (below), but the
    // middleware's builder takes its single option by value, so no caller can drive
    // the published object to zero options: the CEP-8 `>= 1` requirement holds by
    // construction at the only emission site.
    let empty_is_representable = PaymentRequiredErrorData {
        instructions: None,
        payment_options: Vec::new(),
    };
    assert_eq!(
        serde_json::to_value(&empty_is_representable).expect("data serializes")["payment_options"]
            .as_array()
            .expect("array")
            .len(),
        0
    );

    let built = build_payment_required_error(
        serde_json::json!(1),
        PaymentOption {
            amount: 1,
            pmi: "bitcoin-lightning-bolt11".to_string(),
            pay_req: "lnbc1...".to_string(),
            description: None,
            ttl: None,
            meta: None,
        },
    );
    assert_eq!(
        serde_json::to_value(&built).expect("error object serializes")["error"]["data"]
            ["payment_options"]
            .as_array()
            .expect("array")
            .len(),
        1,
        "the builder emits exactly one option, always"
    );
}

#[test]
fn cep8_payment_required_error_data_shape() {
    let data = PaymentRequiredErrorData {
        instructions: None,
        payment_options: vec![PaymentOption {
            amount: 100,
            pmi: "bitcoin-lightning-bolt11".to_string(),
            pay_req: "lnbc1...".to_string(),
            description: None,
            ttl: None,
            meta: None,
        }],
    };
    assert_eq!(
        serde_json::to_value(&data).expect("data serializes"),
        serde_json::json!({
            "payment_options": [{
                "amount": 100,
                "pmi": "bitcoin-lightning-bolt11",
                "pay_req": "lnbc1...",
            }],
        })
    );
}
